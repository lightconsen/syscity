//! Windows Accessibility tool — query UI trees via PowerShell + UIAutomation.
//!
//! Uses the .NET `System.Windows.Automation` namespace to traverse the
//! foreground window's control hierarchy and output it as JSON.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Description of a UI element on Windows.
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
    /// UIA `AutomationId`, when the application publishes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
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

/// Query the Windows accessibility tree using UIAutomation.
///
/// This tool requires PowerShell and the .NET Framework (both present
/// on standard Windows desktops).  On Windows Server Core or stripped
/// builds the UIAutomationClient assembly may be missing — the tool
/// degrades gracefully with an explanatory error.
#[derive(Debug)]
pub struct WindowsAccessibilityTool;

impl Default for WindowsAccessibilityTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsAccessibilityTool {
    pub fn new() -> Self {
        Self
    }

    /// Build a PowerShell script that traverses the foreground window.
    fn build_ui_tree_script() -> String {
        r#"
Add-Type -AssemblyName UIAutomationClient -ErrorAction Stop

function Export-Element {
    param($elem, $depth = 0)
    $r = $elem.Current
    $obj = @{
        role   = $r.ControlType.ProgrammaticName -replace '^ControlType\.',''
        name   = $r.Name
        value  = if ($r.Value -is [string]) { $r.Value } else { $null }
        enabled = $r.IsEnabled
        x      = [int]$r.BoundingRectangle.X
        y      = [int]$r.BoundingRectangle.Y
        width  = [int]$r.BoundingRectangle.Width
        height = [int]$r.BoundingRectangle.Height
        identifier = $r.AutomationId
        children = @()
    }
    if ($depth -lt 3) {
        $children = $elem.FindAll([System.Windows.Automation.TreeScope]::Children,
                                  [System.Windows.Automation.Condition]::TrueCondition)
        for ($i = 0; $i -lt $children.Count; $i++) {
            $obj.children += (Export-Element $children[$i] ($depth + 1))
        }
    }
    return $obj
}

$root = [System.Windows.Automation.AutomationElement]::RootElement
# Try to find the foreground (active) window
$cond = [System.Windows.Automation.ControlTypeCondition]::Window
$allWindows = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $cond)
$target = $null
for ($i = 0; $i -lt $allWindows.Count; $i++) {
    if ($allWindows[$i].Current.NativeWindowHandle -ne 0) {
        $target = $allWindows[$i]
        break
    }
}
if (-not $target -and $allWindows.Count -gt 0) {
    $target = $allWindows[0]
}

if ($target) {
    $tree = Export-Element $target
    $tree | ConvertTo-Json -Depth 10 -Compress
} else {
    '{"role":"desktop","name":"","children":[]}'
}
"#
        .to_string()
    }

    /// Parse the JSON returned by PowerShell into a tree of elements.
    ///
    /// The PowerShell output is already hierarchical (`children` arrays), so
    /// we preserve the nesting instead of flattening it.
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
            identifier: obj
                .get("identifier")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            children,
        })
    }

    /// Build a PowerShell script that performs a native UIA action on the
    /// element at `path` (a child-index path from the target window).
    fn build_action_script(
        action: &str,
        app: Option<&str>,
        path: &[usize],
        value: Option<&str>,
    ) -> String {
        let path_list = path
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // Single quotes inside a PowerShell single-quoted string are doubled.
        let app_pattern = app.unwrap_or("").replace('\'', "''");
        let value_literal = value.unwrap_or("").replace('\'', "''");

        let action_body = match action {
            "press" => r#"
try { $p = $elem.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern); $p.Invoke(); '{"ok":true,"result":"invoked"}' }
catch {
    try { $p = $elem.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern); $p.Select(); '{"ok":true,"result":"selected"}' }
    catch {
        try { $p = $elem.GetCurrentPattern([System.Windows.Automation.LegacyIAccessiblePattern]::Pattern); $p.DoDefaultAction(); '{"ok":true,"result":"default_action"}' }
        catch { '{"ok":false,"error":"element exposes no invoke pattern"}' }
    }
}"#
                .to_string(),
            "focus" => r#"
try { $elem.SetFocus(); '{"ok":true,"result":"focused"}' }
catch { '{"ok":false,"error":"element cannot take focus"}' }"#
                .to_string(),
            "set_value" => format!(
                r#"
try {{ $p = $elem.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern); $p.SetValue('{value_literal}'); '{{"ok":true,"result":"value_set"}}' }}
catch {{ '{{"ok":false,"error":"element has no settable value"}}' }}"#
            ),
            _ => String::new(),
        };

        format!(
            r#"
Add-Type -AssemblyName UIAutomationClient -ErrorAction Stop

$path = @({path_list})
$root = [System.Windows.Automation.AutomationElement]::RootElement
$cond = [System.Windows.Automation.ControlTypeCondition]::Window
$windows = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $cond)

$target = $null
if ('{app_pattern}' -ne '') {{
    for ($i = 0; $i -lt $windows.Count; $i++) {{
        if ($windows[$i].Current.Name -like '*{app_pattern}*') {{ $target = $windows[$i]; break }}
    }}
}}
if (-not $target) {{
    for ($i = 0; $i -lt $windows.Count; $i++) {{
        if ($windows[$i].Current.NativeWindowHandle -ne 0) {{ $target = $windows[$i]; break }}
    }}
}}
if (-not $target -and $windows.Count -gt 0) {{ $target = $windows[0] }}
if (-not $target) {{ '{{"ok":false,"error":"no window"}}'; exit }}

$elem = $target
foreach ($i in $path) {{
    $kids = $elem.FindAll([System.Windows.Automation.TreeScope]::Children,
                          [System.Windows.Automation.Condition]::TrueCondition)
    if ($i -ge $kids.Count) {{ '{{"ok":false,"error":"path out of range"}}'; exit }}
    $elem = $kids[$i]
}}
{action_body}
"#
        )
    }

    /// Run a native UIA action against an element path.
    async fn run_action(action: &str, args: &Value) -> crate::Result<ToolExecutionResult> {
        let Some(path) = args["path"].as_array().and_then(|values| {
            values
                .iter()
                .map(|v| v.as_u64().map(|n| n as usize))
                .collect::<Option<Vec<_>>>()
        }) else {
            return Ok(ToolExecutionResult::error(
                "an accessibility action needs a child-index `path`",
            ));
        };

        let script =
            Self::build_action_script(action, args["app"].as_str(), &path, args["value"].as_str());
        if script.is_empty() {
            return Ok(ToolExecutionResult::error(format!(
                "unsupported accessibility action '{action}'"
            )));
        }

        let (success, stdout, stderr) = Self::run_ps(&script).await?;
        let parsed: Value = serde_json::from_str(&stdout).unwrap_or_else(|_| {
            serde_json::json!({ "ok": false, "error": if stderr.is_empty() { stdout.clone() } else { stderr.clone() } })
        });
        let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);

        if success && ok {
            Ok(ToolExecutionResult::success("ok").with_data(serde_json::json!({
                "success": true,
                "action": action,
                "path": path,
                "result": parsed.get("result").cloned().unwrap_or_default(),
            })))
        } else {
            let error = parsed
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("accessibility action failed")
                .to_string();
            Ok(ToolExecutionResult::error(error.clone()).with_data(serde_json::json!({
                "success": false,
                "action": action,
                "path": path,
                "error": error,
            })))
        }
    }

    async fn run_ps(script: &str) -> crate::Result<(bool, String, String)> {
        let output = timeout(
            Duration::from_secs(15),
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
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok((out.status.success(), stdout, stderr))
            }
            Ok(Err(e)) => Ok((false, String::new(), format!("PowerShell spawn error: {}", e))),
            Err(_) => Ok((false, String::new(), "PowerShell timed out".to_string())),
        }
    }
}

#[async_trait]
impl Tool for WindowsAccessibilityTool {
    fn name(&self) -> &str {
        "windows_accessibility"
    }

    fn description(&self) -> &str {
        "Query the Windows accessibility/UIAutomation tree for the active window. Returns a \
         structured list of buttons, text fields, menus, and other UI elements. Use this before \
         taking screenshots whenever possible for structured desktop perception."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Query UI tree via UIAutomation",
            serde_json::json!({
                "app": {
                    "type": "string",
                    "description": "Window title pattern to inspect (partial match). If omitted, inspects the foreground window."
                },
                "action": {
                    "type": "string",
                    "enum": ["tree", "press", "focus", "set_value"],
                    "description": "tree (default) reads the element tree; the others act on the element at `path`."
                },
                "path": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Child-index path to the element: [window, child, grandchild, ...], 0-based."
                },
                "value": {
                    "type": "string",
                    "description": "Value to set, for action=set_value."
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
        if let Some(action) = args["action"].as_str().filter(|a| *a != "tree") {
            return Self::run_action(action, &args).await;
        }

        info!("Querying Windows accessibility tree");

        let (success, stdout, stderr) = Self::run_ps(&Self::build_ui_tree_script()).await?;

        let mut result = AccessibilityResult {
            success,
            app: None,
            elements: Vec::new(),
            raw_output: Some(stdout.clone()),
            error: if success { None } else { Some(stderr.clone()) },
        };

        if success {
            match serde_json::from_str::<Value>(&stdout) {
                Ok(json) => {
                    result.app = json.get("name").and_then(|v| v.as_str()).map(String::from);
                    result.elements = Self::parse_json_tree(&json);
                }
                Err(e) => {
                    warn!("Failed to parse UIAutomation JSON output: {}", e);
                    result.success = false;
                    result.error = Some(format!("JSON parse error: {}", e));
                }
            }
        } else {
            warn!("UIAutomation query failed: {}", stderr);
        }

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
        cfg!(target_os = "windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accessibility_tool_creation() {
        let tool = WindowsAccessibilityTool::new();
        assert_eq!(tool.name(), "windows_accessibility");
        assert!(tool.description().contains("UIAutomation"));
    }

    #[test]
    fn test_parse_json_tree() {
        let json = serde_json::json!({
            "role": "Window",
            "name": "Calculator",
            "enabled": true,
            "x": 0, "y": 0, "width": 400, "height": 600,
            "children": [
                {
                    "role": "Button",
                    "name": "7",
                    "enabled": true,
                    "x": 10, "y": 100, "width": 50, "height": 50,
                    "children": []
                },
                {
                    "role": "Edit",
                    "name": "Display",
                    "value": "0",
                    "enabled": true,
                    "x": 10, "y": 10, "width": 380, "height": 40,
                    "children": []
                }
            ]
        });

        let elements = WindowsAccessibilityTool::parse_json_tree(&json);
        assert_eq!(elements.len(), 1, "should have 1 top-level window");
        assert_eq!(elements[0].role, "Window");
        assert_eq!(elements[0].children.len(), 2);
        assert_eq!(elements[0].children[0].role, "Button");
        assert_eq!(elements[0].children[1].role, "Edit");
        assert_eq!(elements[0].children[1].value.as_deref(), Some("0"));
    }
}
