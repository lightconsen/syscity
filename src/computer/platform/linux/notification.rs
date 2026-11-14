//! Linux notification tool — send and monitor desktop notifications.
//!
//! Uses `notify-send` (libnotify) for sending and `dbus-monitor` for
//! best-effort listening.  Works on both X11 and Wayland because the
//! freedesktop Notifications D-Bus service is display-server agnostic.

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
    /// Display a desktop notification.
    Send,
    /// Monitor incoming notifications for a short period.
    Listen,
}

/// A captured notification entry.
#[derive(Debug, Clone, Serialize)]
pub struct NotificationEntry {
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub urgency: String,
}

/// Tool for sending and monitoring desktop notifications on Linux.
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

    async fn do_send(
        title: &str,
        message: &str,
        urgency: &str,
        icon: Option<&str>,
    ) -> (bool, String) {
        let mut args = vec![
            "--urgency",
            match urgency {
                "low" => "low",
                "critical" => "critical",
                _ => "normal",
            },
            title,
            message,
        ];

        let icon_arg;
        if let Some(ic) = icon {
            icon_arg = format!("--icon={}", ic);
            args.push(&icon_arg);
        }

        match Self::run_cmd("notify-send", &args, 10).await {
            Some((success, output)) => (success, output),
            None => (false, "Failed to execute notify-send".to_string()),
        }
    }

    async fn do_listen(duration_secs: u64, max_count: usize) -> Vec<NotificationEntry> {
        // Best-effort: use dbus-monitor to watch the Notifications interface.
        // notify-send itself does not support listening.
        let dbus_cmd =
            "dbus-monitor \"interface='org.freedesktop.Notifications'\" 2>/dev/null".to_string();

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let child_result = timeout(
            Duration::from_secs(duration_secs),
            Command::new(&shell).arg("-c").arg(&dbus_cmd).output(),
        )
        .await;

        let output = match child_result {
            Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).to_string(),
            _ => {
                warn!("dbus-monitor failed or timed out for notification listening");
                return Vec::new();
            }
        };

        let mut entries = Vec::new();
        let mut current_app = String::new();
        let mut current_summary = String::new();
        let mut current_body = String::new();
        let mut current_urgency = "normal".to_string();

        for line in output.lines() {
            let trimmed = line.trim();

            // dbus-monitor output for Notify method:
            //   string "app-name"
            //   uint32 0
            //   string "icon"
            //   string "summary"
            //   string "body"
            //   ...
            if trimmed.starts_with("string \"") {
                if let Some(val) = extract_quoted_string(trimmed) {
                    if current_app.is_empty() {
                        current_app = val;
                    } else if current_summary.is_empty() {
                        current_summary = val;
                    } else if current_body.is_empty() {
                        current_body = val;
                    }
                }
            } else if trimmed.starts_with("int32 ") || trimmed.starts_with("uint32 ") {
                // urgency hint sometimes appears as variant int32 N
                if trimmed.contains("int32 2") {
                    current_urgency = "critical".to_string();
                } else if trimmed.contains("int32 0") {
                    current_urgency = "low".to_string();
                }
            }

            // End of a Notify call — push entry when we see the method return
            if trimmed.starts_with("method return") && !current_app.is_empty() {
                entries.push(NotificationEntry {
                    app_name: current_app.clone(),
                    summary: current_summary.clone(),
                    body: current_body.clone(),
                    urgency: current_urgency.clone(),
                });

                if entries.len() >= max_count {
                    break;
                }

                current_app.clear();
                current_summary.clear();
                current_body.clear();
                current_urgency = "normal".to_string();
            }
        }

        entries
    }
}

fn extract_quoted_string(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let rest = &line[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[async_trait]
impl Tool for NotificationTool {
    fn name(&self) -> &str {
        "linux_notification"
    }

    fn description(&self) -> &str {
        "Send desktop notifications or monitor incoming notifications on Linux. Uses notify-send \
         for sending and dbus-monitor for listening. Works on both X11 and Wayland."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::notice(&["linux", "admin", "notification"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Send or monitor desktop notifications",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: 'send' to display a notification, 'listen' to capture incoming notifications",
                    "enum": ["send", "listen"]
                },
                "title": {
                    "type": "string",
                    "description": "Notification title (required for 'send')"
                },
                "message": {
                    "type": "string",
                    "description": "Notification body message (required for 'send')"
                },
                "urgency": {
                    "type": "string",
                    "description": "Urgency level for 'send'",
                    "enum": ["low", "normal", "critical"],
                    "default": "normal"
                },
                "icon": {
                    "type": "string",
                    "description": "Optional icon name or path for 'send'"
                },
                "duration_secs": {
                    "type": "integer",
                    "description": "How long to listen for notifications (default: 10)",
                    "default": 10
                },
                "max_count": {
                    "type": "integer",
                    "description": "Max notifications to capture while listening (default: 5)",
                    "default": 5
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
        let action = match action_str {
            "listen" => NotificationAction::Listen,
            _ => NotificationAction::Send,
        };

        let data = match action {
            NotificationAction::Send => {
                let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let urgency = args
                    .get("urgency")
                    .and_then(|v| v.as_str())
                    .unwrap_or("normal");
                let icon = args.get("icon").and_then(|v| v.as_str());

                if title.is_empty() || message.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "'title' and 'message' are required for send action".to_string(),
                    ));
                }

                info!("Sending notification: {} — {}", title, message);
                let (success, output) = Self::do_send(title, message, urgency, icon).await;
                serde_json::json!({
                    "action": "send",
                    "success": success,
                    "output": output,
                })
            }
            NotificationAction::Listen => {
                let duration = args
                    .get("duration_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10);
                let max_count =
                    args.get("max_count").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

                info!("Listening for notifications for {}s", duration);
                let entries = Self::do_listen(duration, max_count).await;
                serde_json::json!({
                    "action": "listen",
                    "duration_secs": duration,
                    "count": entries.len(),
                    "entries": entries,
                })
            }
        };

        let message = format!("Notification '{}' completed", action_str);
        Ok(ToolExecutionResult::success(message).with_data(data))
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "linux")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notification_tool_name() {
        let tool = NotificationTool::new();
        assert_eq!(tool.name(), "linux_notification");
    }

    #[test]
    fn test_notification_tool_schema() {
        let tool = NotificationTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_extract_quoted_string() {
        assert_eq!(extract_quoted_string(r#"  string "my-app" "#), Some("my-app".to_string()));
        assert_eq!(extract_quoted_string(r#"string """#), Some("".to_string()));
        assert_eq!(extract_quoted_string("no quotes"), None);
    }
}
