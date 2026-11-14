//! Windows clipboard tool using PowerShell.

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::info;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Read from or write to the Windows clipboard using PowerShell.
#[derive(Debug)]
pub struct ClipboardTool;

impl Default for ClipboardTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ClipboardTool {
    fn name(&self) -> &str {
        "windows_clipboard"
    }

    fn description(&self) -> &str {
        "Read from or write to the Windows clipboard using PowerShell. Supports text content. Use \
         'get' to read, 'set' to write."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::clipboard_write(&[
            "computer",
            "windows",
            "clipboard",
        ])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read or write clipboard content",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: 'get' to read, 'set' to write",
                    "enum": ["get", "set"]
                },
                "text": {
                    "type": "string",
                    "description": "Text to write (required for 'set')"
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
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("get");
        info!("Windows clipboard action: {}", action);

        match action {
            "get" => {
                let script = r#"
Add-Type -AssemblyName System.Windows.Forms
$text = [System.Windows.Forms.Clipboard]::GetText()
if ($text -eq $null -or $text -eq '') {
    "[EMPTY]"
} else {
    $text
}
"#;
                let output = timeout(
                    Duration::from_secs(5),
                    Command::new("powershell")
                        .args([
                            "-NoProfile",
                            "-ExecutionPolicy",
                            "Bypass",
                            "-Command",
                            script,
                        ])
                        .output(),
                )
                .await;

                match output {
                    Ok(Ok(out)) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        let display = if text == "[EMPTY]" {
                            "Clipboard is empty".to_string()
                        } else {
                            format!(
                                "Clipboard ({} chars): {}",
                                text.len(),
                                if text.len() > 200 {
                                    format!("{}...", &text[..200])
                                } else {
                                    text.clone()
                                }
                            )
                        };
                        Ok(ToolExecutionResult::success(display).with_data(serde_json::json!({
                            "text": text,
                            "length": text.len()
                        })))
                    }
                    Ok(Ok(out)) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        Ok(ToolExecutionResult::error(format!("Get-Clipboard failed: {}", stderr)))
                    }
                    Ok(Err(e)) => {
                        Ok(ToolExecutionResult::error(format!("Failed to run PowerShell: {}", e)))
                    }
                    Err(_) => {
                        Ok(ToolExecutionResult::error("Clipboard read timed out".to_string()))
                    }
                }
            }
            "set" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let script = format!(
                    r#"
Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Clipboard]::SetText('{}')
"#,
                    text.replace("'", "''")
                );
                let output = timeout(
                    Duration::from_secs(5),
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
                    Ok(Ok(out)) if out.status.success() => Ok(ToolExecutionResult::success(
                        format!("Clipboard set ({} chars)", text.len()),
                    )),
                    Ok(Ok(out)) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        Ok(ToolExecutionResult::error(format!("Set-Clipboard failed: {}", stderr)))
                    }
                    Ok(Err(e)) => {
                        Ok(ToolExecutionResult::error(format!("Failed to run PowerShell: {}", e)))
                    }
                    Err(_) => {
                        Ok(ToolExecutionResult::error("Clipboard write timed out".to_string()))
                    }
                }
            }
            _ => Ok(ToolExecutionResult::error(format!(
                "Unknown action: {}. Use 'get' or 'set'.",
                action
            ))),
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
    fn test_clipboard_tool_creation() {
        let tool = ClipboardTool::new();
        assert_eq!(tool.name(), "windows_clipboard");
    }
}
