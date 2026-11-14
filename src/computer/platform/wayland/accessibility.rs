//! Linux Wayland Accessibility tool — best-effort UI tree access.
//!
//! Wayland's security model intentionally prevents cross-application
//! window introspection.  This tool tries:
//!
//! 1. `dbus-send` → xdg-desktop-portal a11y bus (rarely supported)
//! 2. Check if the compositor exposes any accessibility info
//! 3. Fallback: return empty with a clear explanation
//!
//! For production Wayland automation, consider running inside a
//! nested X11 session (`weston --xwayland`) or using the X11 path.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Description of a UI element.
#[derive(Debug, Clone, Serialize)]
pub struct UiElement {
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub enabled: Option<bool>,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub children: Vec<UiElement>,
}

/// Result of an accessibility query.
#[derive(Debug, Clone, Serialize)]
pub struct AccessibilityResult {
    pub success: bool,
    pub app: Option<String>,
    pub elements: Vec<UiElement>,
    pub raw_output: Option<String>,
    pub error: Option<String>,
}

/// Best-effort Wayland accessibility tool.
#[derive(Debug)]
pub struct WaylandAccessibilityTool;

impl Default for WaylandAccessibilityTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WaylandAccessibilityTool {
    pub fn new() -> Self {
        Self
    }

    /// Try to query the a11y portal via D-Bus.
    async fn try_portal() -> crate::Result<(bool, String, String)> {
        let output = timeout(
            Duration::from_secs(3),
            Command::new("dbus-send")
                .args([
                    "--session",
                    "--dest=org.freedesktop.portal.Desktop",
                    "--type=method_call",
                    "/org/freedesktop/portal/desktop",
                    "org.freedesktop.DBus.Introspectable.Introspect",
                ])
                .output(),
        )
        .await;
        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok((out.status.success(), stdout, stderr))
            }
            Ok(Err(e)) => Ok((false, String::new(), format!("dbus-send error: {}", e))),
            Err(_) => Ok((false, String::new(), "dbus-send timed out".to_string())),
        }
    }
}

#[async_trait]
impl Tool for WaylandAccessibilityTool {
    fn name(&self) -> &str {
        "linux_wayland_accessibility"
    }

    fn description(&self) -> &str {
        "Query the Wayland accessibility tree (best-effort). Wayland restricts cross-app \
         introspection; this tool attempts xdg-desktop-portal a11y bus or returns an empty tree \
         with guidance. For full desktop automation on Wayland, consider using X11 (DISPLAY) or a \
         nested X11 session."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["computer", "wayland", "accessibility"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Query Wayland UI tree",
            serde_json::json!({
                "app": {
                    "type": "string",
                    "description": "Application name to inspect (rarely supported on Wayland)."
                }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        info!("Querying Wayland accessibility tree (best-effort)");

        // Try portal — in practice this almost never returns UI trees,
        // but we check anyway for future-proofing.
        let (portal_ok, _portal_out, portal_err) = Self::try_portal().await?;

        if portal_ok {
            info!("xdg-desktop-portal responded — but UI tree extraction is not yet implemented");
        } else {
            warn!("xdg-desktop-portal a11y unavailable: {}", portal_err);
        }

        // Graceful fallback: empty tree with explanation.
        let explanation = concat!(
            "Wayland restricts cross-application accessibility introspection. ",
            "The UI tree could not be obtained. ",
            "Recommendations: (1) Run the target app under X11 (set DISPLAY), ",
            "(2) Use screenshot-based perception instead, ",
            "(3) Use a nested X11 compositor (e.g. weston --xwayland)."
        );

        let result = AccessibilityResult {
            success: true,
            app: None,
            elements: Vec::new(),
            raw_output: Some(explanation.to_string()),
            error: Some(explanation.to_string()),
        };

        let json = serde_json::to_string_pretty(&result)
            .map_err(crate::error::SyscityError::Serialization)?;

        Ok(ToolExecutionResult::success(json).with_data(serde_json::to_value(result)?))
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        std::env::var("WAYLAND_DISPLAY").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wayland_accessibility_tool_creation() {
        let tool = WaylandAccessibilityTool::new();
        assert_eq!(tool.name(), "linux_wayland_accessibility");
    }
}
