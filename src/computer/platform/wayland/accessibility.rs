//! Linux Wayland Accessibility tool — the AT-SPI2 tree, read honestly.
//!
//! Wayland's security model stops one application from *driving* another's
//! windows, but it does not hide the accessibility tree: GNOME and KDE publish
//! the same AT-SPI2 bus under Wayland that they publish under X11, and
//! `pyatspi` reads it the same way. So the tree is available here, and this
//! tool reads it with the shared reader in
//! [`crate::computer::platform::atspi`].
//!
//! # What this replaced, and why it mattered
//!
//! The previous version asked `xdg-desktop-portal` for an accessibility API.
//! No such portal interface exists — the portal set covers screenshots, input
//! *injection* and screencasting, not introspection — so the query could never
//! succeed, and the tool then returned `success: true` with an empty element
//! list and an explanation. Downstream, `tree_is_complete(&[])` is `true`, so
//! the run loop read that as "this desktop has no controls on it" rather than
//! "this desktop could not be read": the agent was told, in the tree's own
//! format, that there was nothing to look at. Every consumer that tries the
//! tree before falling back to pixels was being starved by a lie shaped like
//! an answer.
//!
//! The honest contract is the one the X11 and Windows tools already keep: a
//! read that produced a tree is `success`, a read that could not is a failure
//! carrying its reason. `computer.play`'s ambiguity preflight and the run
//! loop's "attach an image because the tree is incomplete" rule both key off
//! exactly that distinction.
//!
//! # No `xdotool` fallback here
//!
//! The X11 tool's last resort shell out to `xdotool`/`xwininfo`, which speak
//! X11 and report window geometry rather than controls. Wayland has neither,
//! and a geometry-only "tree" is not evidence a click target exists. Leaving
//! the fallback out is the honest choice: a Wayland screen either reads over
//! AT-SPI or the caller is told it did not.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

use super::super::atspi;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Description of a UI element, in the same shape the X11 tool emits.
///
/// Kept identical to the X11 type rather than shared, because the two tools
/// are registered independently and a caller parses whichever one answered.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<u32>,
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

/// Wayland accessibility, via the same AT-SPI2 bus the X11 tool reads.
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

    /// Parse the reader's JSON into the tool's element shape.
    ///
    /// The AT-SPI2 output is already hierarchical (`children` arrays), so the
    /// nesting is preserved rather than flattened.
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
impl Tool for WaylandAccessibilityTool {
    fn name(&self) -> &str {
        "linux_wayland_accessibility"
    }

    fn description(&self) -> &str {
        "Query the Linux Wayland accessibility tree via AT-SPI2 (GNOME/KDE expose it under \
         Wayland as well as X11). Returns structured UI elements from the active application. \
         A tree that cannot be read is reported as a failure with its reason, never as an \
         empty tree."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["computer", "wayland", "accessibility"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Query Wayland UI tree via AT-SPI2",
            serde_json::json!({
                "app": {
                    "type": "string",
                    "description": "Application name to inspect. If omitted, inspects the focused application."
                },
                "max_depth": {
                    "type": "integer",
                    "description": "How deep to walk the tree (1-16, default 3)."
                }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let max_depth = args["max_depth"]
            .as_u64()
            .map(|d| d.clamp(1, 16) as u32)
            .unwrap_or(3);

        info!("Querying Wayland accessibility tree via AT-SPI2 (depth {max_depth})");
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

        // No tree. Say so, and say what would make it readable — an empty
        // success is what made a Wayland desktop look like a blank one.
        let reason = read
            .error
            .unwrap_or_else(|| "the accessibility read failed".to_string());
        warn!("Wayland AT-SPI2 read failed: {}", reason);
        let explanation = format!(
            "{reason}. The AT-SPI2 accessibility bus is how a Wayland session exposes its UI \
             tree; GNOME and KDE publish it, and `python3-pyatspi` (or `at-spi2-core`) is what \
             reads it. Without it, act by screen coordinates instead."
        );
        let result = AccessibilityResult {
            success: false,
            app: None,
            elements: Vec::new(),
            raw_output: None,
            error: Some(explanation.clone()),
        };
        Ok(ToolExecutionResult::error(explanation).with_data(serde_json::to_value(result)?))
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

    /// The tool reads the same AT-SPI2 script the X11 tool does, because that
    /// bus is not an X11 feature. If this ever diverges, one of the two
    /// sessions silently loses its tree.
    #[test]
    fn it_reads_the_shared_atspi_script() {
        let script = atspi::python_script(3);
        assert!(script.contains("import pyatspi"), "{script}");
        assert!(script.contains("STATE_ACTIVE"), "{script}");
    }

    #[test]
    fn a_tree_is_parsed_with_its_nesting_intact() {
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

        let elements = WaylandAccessibilityTool::parse_json_tree(&json);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].role, "frame");
        assert_eq!(elements[0].children.len(), 1);
        assert_eq!(elements[0].children[0].name, "Back");
    }

    /// The completeness fields survive parsing: a caller deciding whether to
    /// re-read deeper needs them, and a Wayland tree is the one most likely to
    /// be cut by depth.
    #[test]
    fn completeness_survives_parsing() {
        let json = serde_json::json!({
            "role": "frame",
            "name": "App",
            "children_count": 9,
            "truncated": true,
            "children": []
        });
        let elements = WaylandAccessibilityTool::parse_json_tree(&json);
        assert_eq!(elements[0].children_count, Some(9));
        assert!(elements[0].truncated, "a cut walk says so");
    }
}
