//! Linux X11 clipboard tool using `xclip`.

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Read from or write to the X11 clipboard using `xclip`.
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
        "linux_x11_clipboard"
    }

    fn description(&self) -> &str {
        "Read from or write to the X11 clipboard using xclip. Supports text content. Use 'get' to \
         read, 'set' to write."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::clipboard_write(&["computer", "x11", "clipboard"])
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

        info!("X11 clipboard action: {}", action);

        match action {
            "get" => {
                let output = timeout(
                    Duration::from_secs(5),
                    Command::new("xclip")
                        .args(["-selection", "clipboard", "-o"])
                        .output(),
                )
                .await;

                match output {
                    Ok(Ok(out)) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout);
                        Ok(ToolExecutionResult::success(format!(
                            "Clipboard content ({} chars): {}",
                            text.len(),
                            if text.len() > 200 {
                                format!("{}...", &text[..200])
                            } else {
                                text.to_string()
                            }
                        ))
                        .with_data(serde_json::json!({
                            "text": text.as_ref(),
                            "length": text.len()
                        })))
                    }
                    Ok(Ok(out)) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        Ok(ToolExecutionResult::error(format!("xclip failed: {}", stderr)))
                    }
                    Ok(Err(e)) => {
                        Ok(ToolExecutionResult::error(format!("Failed to run xclip: {}", e)))
                    }
                    Err(_) => Ok(ToolExecutionResult::error("xclip timed out".to_string())),
                }
            }
            "set" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let output = timeout(Duration::from_secs(5), async {
                    let mut child = Command::new("xclip")
                        .args(["-selection", "clipboard", "-i"])
                        .stdin(std::process::Stdio::piped())
                        .spawn()?;
                    use tokio::io::AsyncWriteExt;
                    if let Some(mut stdin) = child.stdin.take() {
                        if let Err(e) = stdin.write_all(text.as_bytes()).await {
                            warn!("Failed to write to xclip stdin: {}", e);
                        }
                        if let Err(e) = stdin.shutdown().await {
                            warn!("Failed to shutdown xclip stdin: {}", e);
                        }
                    }
                    child.wait().await
                })
                .await;

                match output {
                    Ok(Ok(status)) if status.success() => Ok(ToolExecutionResult::success(
                        format!("Clipboard set ({} chars)", text.len()),
                    )),
                    Ok(Ok(_)) => {
                        Ok(ToolExecutionResult::error("xclip exited with error".to_string()))
                    }
                    Ok(Err(e)) => {
                        Ok(ToolExecutionResult::error(format!("xclip process error: {}", e)))
                    }
                    Err(_) => Ok(ToolExecutionResult::error("xclip timed out".to_string())),
                }
            }
            _ => Ok(ToolExecutionResult::error(format!(
                "Unknown action: {}. Use 'get' or 'set'.",
                action
            ))),
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        std::env::var("DISPLAY").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clipboard_tool_creation() {
        let tool = ClipboardTool::new();
        assert_eq!(tool.name(), "linux_x11_clipboard");
    }
}
