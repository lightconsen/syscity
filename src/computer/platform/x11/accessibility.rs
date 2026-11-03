//! Linux X11 Accessibility tool — query UI trees via AT-SPI2.
//!
//! Tries `python3` + `pyatspi` first, then falls back to `atspi2` D-Bus
//! introspection, then finally to `xdotool` + `xwininfo` for minimal
//! window information.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

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

    /// Python script that traverses the active AT-SPI2 application.
    ///
    /// `max_depth` is substituted into the script rather than frozen at 3: a
    /// caller asking for a deeper tree used to be silently given a shallow
    /// one, which reads as "the element is not there".
    fn build_python_script(max_depth: u32) -> String {
        // The script is a template rather than a `format!` string: its Python
        // bodies are full of `{}` dict literals, which would have to be
        // doubled to survive formatting.
        r#"
import json, sys
try:
    import pyatspi
except ImportError:
    print(json.dumps({"error": "pyatspi not installed"}), file=sys.stderr)
    sys.exit(1)

def export_element(acc, depth=0):
    try:
        name = acc.name or ""
        role = acc.getRoleName() or "unknown"
        state_set = acc.getState()
        enabled = state_set.contains(pyatspi.STATE_ENABLED)
        focused = state_set.contains(pyatspi.STATE_FOCUSED)
        x, y, w, h = acc.extents
    except Exception:
        return None
    try:
        child_count = int(acc.childCount)
    except Exception:
        child_count = None
    obj = {
        "role": role,
        "name": name,
        "enabled": enabled,
        "focused": focused,
        "x": x, "y": y,
        "width": w, "height": h,
        "children_count": child_count,
        "truncated": False,
        "children": []
    }
    if depth < __MAX_DEPTH__:
        try:
            for i in range(acc.childCount):
                child = acc.getChildAtIndex(i)
                child_dict = export_element(child, depth + 1)
                if child_dict:
                    obj["children"].append(child_dict)
        except Exception:
            pass
    else:
        # Stopped at the depth limit with children still unread. The count is
        # what makes that visible: the caller can tell "no children" from
        # "children nobody looked at".
        obj["truncated"] = bool(child_count)
    return obj

desktop = pyatspi.Registry.getDesktop(0)
active_app = None
for i in range(desktop.childCount):
    app = desktop.getChildAtIndex(i)
    try:
        if app.getState().contains(pyatspi.STATE_ACTIVE) or app.getState().contains(pyatspi.STATE_FOCUSED):
            active_app = app
            break
    except Exception:
        pass

if not active_app and desktop.childCount > 0:
    active_app = desktop.getChildAtIndex(0)

if active_app:
    tree = export_element(active_app)
    print(json.dumps(tree))
else:
    print(json.dumps({"role": "desktop", "name": "", "children": []}))
"#
        .replace("__MAX_DEPTH__", &max_depth.to_string())
    }

    /// Fallback script using xdotool + xwininfo for minimal window data.
    fn build_xwininfo_fallback() -> &'static str {
        r#"
import json, subprocess, sys

def run(cmd):
    try:
        out = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=5)
        return out.stdout.strip()
    except Exception:
        return ""

# Get active window id
wid = run("xdotool getactivewindow 2>/dev/null || xdotool getwindowfocus 2>/dev/null")
if not wid:
    print(json.dumps({"role":"desktop","name":"","children":[]}))
    sys.exit(0)

# Get window info
info = run(f"xwininfo -id {wid} 2>/dev/null")
name = ""
for line in info.splitlines():
    if "Window id:" in line and '"' in line:
        name = line.split('"')[1]
        break

# Get window geometry
geo = run(f"xdotool getwindowgeometry {wid} 2>/dev/null")
x, y, w, h = 0, 0, 0, 0
for line in geo.splitlines():
    if "Position:" in line:
        parts = line.replace(",", "").split()
        try:
            x = int(parts[1])
            y = int(parts[2])
        except Exception:
            pass
    if "Geometry:" in line:
        parts = line.replace("x", " ").split()
        try:
            w = int(parts[1])
            h = int(parts[2])
        except Exception:
            pass

print(json.dumps({
    "role": "Window",
    "name": name,
    "enabled": True,
    "x": x, "y": y,
    "width": w, "height": h,
    "children": []
}))
"#
    }

    async fn run_python(script: &str) -> crate::Result<(bool, String, String)> {
        let output = timeout(
            Duration::from_secs(10),
            Command::new("python3").arg("-c").arg(script).output(),
        )
        .await;
        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok((out.status.success() && !stdout.is_empty(), stdout, stderr))
            }
            Ok(Err(e)) => Ok((false, String::new(), format!("Python spawn error: {}", e))),
            Err(_) => Ok((false, String::new(), "Python timed out".to_string())),
        }
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
        info!("Querying X11 accessibility tree via AT-SPI2");

        // Strategy 1: Python + pyatspi
        let max_depth = _args["max_depth"]
            .as_u64()
            .map(|d| d.clamp(1, 16) as u32)
            .unwrap_or(3);
        let script = Self::build_python_script(max_depth);
        let (success, stdout, stderr) = Self::run_python(&script).await?;

        if success {
            match serde_json::from_str::<Value>(&stdout) {
                Ok(json) => {
                    let app = json.get("name").and_then(|v| v.as_str()).map(String::from);
                    let elements = Self::parse_json_tree(&json);
                    let result = AccessibilityResult {
                        success: true,
                        app,
                        elements,
                        raw_output: Some(stdout),
                        error: None,
                    };
                    let json_str = serde_json::to_string_pretty(&result)
                        .map_err(crate::error::SyscityError::Serialization)?;
                    return Ok(ToolExecutionResult::success(json_str)
                        .with_data(serde_json::to_value(result)?));
                }
                Err(e) => {
                    warn!("AT-SPI2 JSON parse failed: {}", e);
                }
            }
        } else {
            warn!("AT-SPI2 (pyatspi) failed: {}", stderr);
        }

        // Strategy 2: xdotool + xwininfo fallback
        info!("Falling back to xdotool/xwininfo");
        let (fb_success, fb_stdout, fb_stderr) =
            Self::run_python(Self::build_xwininfo_fallback()).await?;

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
    /// frozen at 3.
    #[test]
    fn the_python_script_honours_the_requested_depth() {
        let script = X11AccessibilityTool::build_python_script(7);
        assert!(script.contains("if depth < 7:"), "{script}");
        assert!(!script.contains("__MAX_DEPTH__"), "placeholder left in script");
        // The dict literals survive templating untouched.
        assert!(script.contains(r#"{"error": "pyatspi not installed"}"#));
        // Focus decides where a bare `type` / `key_press` lands.
        assert!(script.contains("STATE_FOCUSED"), "{script}");
        assert!(script.contains(r#""focused": focused,"#), "{script}");
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
