//! Linux X11 Accessibility tool — query UI trees via AT-SPI2.
//!
//! The AT-SPI2 script lives in [`crate::computer::platform::atspi`] because it
//! is not X11-specific: the same bus serves Wayland sessions. What is X11's
//! own is the last-resort fallback, `xdotool` + `xwininfo`, which reads window
//! geometry and no controls.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

use super::super::atspi;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Description of a UI element on Linux.
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
    /// Whether the element holds keyboard focus. `None` when the read did not
    /// say; it decides where a bare `type` / `key_press` lands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    /// How many children the element has, when AT-SPI2 said. A count larger
    /// than `children.len()` means the walk stopped before reading them all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<u32>,
    /// Whether the depth limit stopped this node's walk with children unread.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
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

/// Query the Linux X11 accessibility tree using AT-SPI2.
///
/// Priority:
/// 1. `python3` + `pyatspi` (most detailed tree)
/// 2. `dbus-send` AT-SPI2 bus (medium detail)
/// 3. `xdotool` + `xwininfo` (minimal window info only)
#[derive(Debug)]
pub struct X11AccessibilityTool;

impl Default for X11AccessibilityTool {
    fn default() -> Self {
        Self::new()
    }
}

impl X11AccessibilityTool {
    pub fn new() -> Self {
        Self
    }

    /// The X11-only fallback script.
    fn build_xwininfo_fallback() -> &'static str {
        atspi::xwininfo_fallback_script()
    }

    /// Parse JSON tree into a tree of elements.
    ///
    /// The pyatspi/xdotool output is already hierarchical (`children` arrays),
    /// so we preserve the nesting instead of flattening it.
    fn parse_json_tree(value: &Value) -> Vec<UiElement> {
        Self::parse_element(value).into_iter().collect()
    }

    fn parse_element(value: &Value) -> Option<UiElement> {
        let obj = value.as_object()?;
        let children = obj
            .get("children")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(Self::parse_element).collect())
            .unwrap_or_default();

        Some(UiElement {
            role: obj
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            name: obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            value: obj.get("value").and_then(|v| v.as_str()).map(String::from),
            enabled: obj.get("enabled").and_then(|v| v.as_bool()),
            x: obj.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            y: obj.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            width: obj.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            height: obj.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            focused: obj.get("focused").and_then(|v| v.as_bool()),
            children_count: obj
                .get("children_count")
                .and_then(|v| v.as_u64())
                .map(|v| v.min(u32::MAX as u64) as u32),
            truncated: obj
                .get("truncated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            children,
        })
    }
}

#[async_trait]
impl Tool for X11AccessibilityTool {
    fn name(&self) -> &str {
        "linux_x11_accessibility"
    }

    fn description(&self) -> &str {
        "Query the Linux X11 accessibility tree via AT-SPI2. Returns structured UI elements \
         (buttons, text fields, etc.) from the active application.  Falls back to xdotool window \
         info if AT-SPI2 is unavailable."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["computer", "x11", "accessibility"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Query X11 UI tree via AT-SPI2",
            serde_json::json!({
                "app": {
                    "type": "string",
                    "description": "Application name to inspect. If omitted, inspects the focused application."
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
        // Strategy 1: the shared AT-SPI2 read (the same one the Wayland tool
        // uses — the bus is not an X11 feature).
        info!("Querying X11 accessibility tree via AT-SPI2");
        let max_depth = _args["max_depth"]
            .as_u64()
            .map(|d| d.clamp(1, 16) as u32)
            .unwrap_or(3);
        let read = atspi::read(max_depth).await;

        if read.ok {
            let json = read.tree.unwrap_or(Value::Null);
            let elements = Self::parse_json_tree(&json);
            let result = AccessibilityResult {
                success: true,
                app: read.app.clone(),
                elements,
                raw_output: read.raw.clone(),
                error: None,
            };
            let json_str = serde_json::to_string_pretty(&result)
                .map_err(crate::error::SyscityError::Serialization)?;
            return Ok(
                ToolExecutionResult::success(json_str).with_data(serde_json::to_value(result)?)
            );
        }
        let stderr = read.error.unwrap_or_default();
        warn!("AT-SPI2 (pyatspi) failed: {}", stderr);

        // Strategy 2: xdotool + xwininfo fallback
        info!("Falling back to xdotool/xwininfo");
        let (fb_success, fb_stdout, fb_stderr) =
            atspi::run_python(Self::build_xwininfo_fallback()).await?;

        if fb_success {
            match serde_json::from_str::<Value>(&fb_stdout) {
                Ok(json) => {
                    let app = json.get("name").and_then(|v| v.as_str()).map(String::from);
                    let elements = Self::parse_json_tree(&json);
                    let result = AccessibilityResult {
                        success: true,
                        app,
                        elements,
                        raw_output: Some(fb_stdout),
                        error: Some(format!("AT-SPI2 fallback; original: {}", stderr)),
                    };
                    let json_str = serde_json::to_string_pretty(&result)
                        .map_err(crate::error::SyscityError::Serialization)?;
                    return Ok(ToolExecutionResult::success(json_str)
                        .with_data(serde_json::to_value(result)?));
                }
                Err(e) => {
                    warn!("Fallback JSON parse failed: {}", e);
                }
            }
        }

        // All strategies failed
        let err_msg =
            format!("AT-SPI2 unavailable and xdotool fallback failed: {} / {}", stderr, fb_stderr);
        let result = AccessibilityResult {
            success: false,
            app: None,
            elements: Vec::new(),
            raw_output: None,
            error: Some(err_msg.clone()),
        };
        Ok(ToolExecutionResult::error(err_msg).with_data(serde_json::to_value(result)?))
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        std::env::var("DISPLAY").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_x11_accessibility_tool_creation() {
        let tool = X11AccessibilityTool::new();
        assert_eq!(tool.name(), "linux_x11_accessibility");
    }

    /// The depth the caller asked for reaches the walker rather than being
    /// frozen at 3. The script lives in `atspi` now, so this asserts the
    /// delegation rather than re-testing the template (which `atspi`'s own
    /// tests cover).
    #[test]
    fn the_requested_depth_reaches_the_shared_reader() {
        let script = atspi::python_script(7);
        assert!(script.contains("if depth < 7:"), "{script}");
    }

    #[test]
    fn test_parse_json_tree() {
        let json = serde_json::json!({
            "role": "frame",
            "name": "Firefox",
            "enabled": true,
            "x": 0, "y": 0, "width": 1280, "height": 720,
            "children": [
                {
                    "role": "push button",
                    "name": "Back",
                    "enabled": true,
                    "x": 10, "y": 40, "width": 40, "height": 30,
                    "children": []
                }
            ]
        });

        let elements = X11AccessibilityTool::parse_json_tree(&json);
        assert_eq!(elements.len(), 1, "should have 1 top-level frame");
        assert_eq!(elements[0].role, "frame");
        assert_eq!(elements[0].children.len(), 1);
        assert_eq!(elements[0].children[0].role, "push button");
    }
}
