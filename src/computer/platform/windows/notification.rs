//! Windows notification tool — send desktop notifications.
//!
//! Uses PowerShell with Windows Runtime APIs (Windows.UI.Notifications)
//! to display toast notifications.  Listening to notifications is not
//! supported without a dedicated Win32 COM listener.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action types for notification management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAction {
    /// Display a desktop (toast) notification.
    Send,
    /// Monitor incoming notifications (not supported on Windows via
    /// PowerShell).
    Listen,
}

/// Tool for sending desktop notifications on Windows.
#[derive(Debug)]
pub struct NotificationTool;

impl Default for NotificationTool {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationTool {
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

    async fn do_send(title: &str, message: &str) -> (bool, String) {
        // Use PowerShell to create a Windows toast notification via
        // the System.Windows.Forms NotifyIcon (works on all Windows versions).
        let ps_script = format!(
            r#"
            Add-Type -AssemblyName System.Windows.Forms
            $icon = New-Object System.Windows.Forms.NotifyIcon
            $icon.Icon = [System.Drawing.SystemIcons]::Information
            $icon.BalloonTipTitle = "{}"
            $icon.BalloonTipText = "{}"
            $icon.Visible = $true
            $icon.ShowBalloonTip(5000)
            Start-Sleep -Milliseconds 200
            $icon.Dispose()
            "#,
            title.replace('"', "`\""),
            message.replace('"', "`\""),
        );

        match Self::run_cmd("powershell", &["-NoProfile", "-Command", &ps_script], 15).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute PowerShell notification script".to_string()),
        }
    }
}

#[async_trait]
impl Tool for NotificationTool {
    fn name(&self) -> &str {
        "windows_notification"
    }

    fn description(&self) -> &str {
        "Send desktop (toast) notifications on Windows using PowerShell. Supports title and \
         message. Listening to notifications is not available on Windows."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::notice(&["computer", "windows", "notification"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Send a desktop notification",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: 'send' to display a notification (listen is not supported)",
                    "enum": ["send", "listen"]
                },
                "title": {
                    "type": "string",
                    "description": "Notification title (required for 'send')"
                },
                "message": {
                    "type": "string",
                    "description": "Notification body message (required for 'send')"
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
            .unwrap_or("send");

        if action_str == "listen" {
            return Ok(ToolExecutionResult::error(
                "Listening to notifications is not supported on Windows via PowerShell. Use \
                 'send' to display a notification instead."
                    .to_string(),
            ));
        }

        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");

        if title.is_empty() || message.is_empty() {
            return Ok(ToolExecutionResult::error(
                "'title' and 'message' are required for send action".to_string(),
            ));
        }

        info!("Sending Windows notification: {} — {}", title, message);
        let (success, output) = Self::do_send(title, message).await;

        let data = serde_json::json!({
            "action": "send",
            "success": success,
            "output": output,
        });

        let message = "Notification send completed".to_string();
        if success {
            Ok(ToolExecutionResult::success(message).with_data(data))
        } else {
            Ok(ToolExecutionResult::error(output).with_data(data))
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
    fn test_notification_tool_name() {
        let tool = NotificationTool::new();
        assert_eq!(tool.name(), "windows_notification");
    }

    #[test]
    fn test_notification_tool_schema() {
        let tool = NotificationTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }
}
