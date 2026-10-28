//! macOS Accessibility tool — query UI trees via AppleScript/System Events.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

use super::applescript::AppleScriptTool;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Description of a UI element on macOS.
#[derive(Debug, Clone, Serialize)]
pub struct UiElement {
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub enabled: Option<bool>,
    pub position: Option<String>,
    pub size: Option<String>,
    /// `AXIdentifier`, when the application publishes one. It is stable
    /// across snapshots, which makes it a better handle than a child path.
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

/// Query the macOS accessibility tree using AppleScript/System Events.
///
/// This is the first step of the hybrid desktop perception model:
/// structured UI tree first, screenshot only when needed.
#[derive(Debug)]
pub struct AccessibilityTool;

impl Default for AccessibilityTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessibilityTool {
    pub fn new() -> Self {
        Self
    }

    /// Build an AppleScript that enumerates UI elements of a target
    /// application.
    fn build_ui_tree_script(app_name: &str) -> String {
        format!(
            r#"tell application "System Events"
    tell application process "{}"
        set frontmost to true
        set _output to "app|{}" & "\n"
        set _indent to "  "

        repeat with _win in (get every window)
            set _output to _output & "window|" & (name of _win) & "|enabled|" & (enabled of _win) & "\n"
            set _output to _output & my describe_elements(_win, _indent)
        end repeat

        return _output
    end tell
end tell

on describe_elements(_container, _indent)
    tell application "System Events"
        set _result to ""
        try
            repeat with _elem in (get every UI element of _container)
                try
                    set _role to role of _elem as string
                on error
                    set _role to "unknown"
                end try
                try
                    set _name to name of _elem as string
                on error
                    set _name to ""
                end try
                try
                    set _value to value of _elem as string
                on error
                    set _value to ""
                end try
                try
                    set _enabled to enabled of _elem as string
                on error
                    set _enabled to ""
                end try
                try
                    set _pos to position of _elem as string
                on error
                    set _pos to ""
                end try
                try
                    set _sz to size of _elem as string
                on error
                    set _sz to ""
                end try
                try
                    set _idn to value of attribute "AXIdentifier" of _elem as string
                on error
                    set _idn to ""
                end try

                set _result to _result & _indent & _role & "|" & _name & "|" & _value & "|" & _enabled & "|" & _pos & "|" & _sz & "|" & _idn & "\n"
                set _result to _result & my describe_elements(_elem, _indent & "  ")
            end repeat
        end try
        return _result
    end tell
end describe_elements"#,
            app_name, app_name
        )
    }

    fn build_frontmost_script() -> String {
        r#"tell application "System Events"
    set _proc to first application process whose frontmost is true
    set _name to name of _proc
    set _output to "app|" & _name & "\n"

    tell _proc
        repeat with _win in (get every window)
            try
                set _win_name to name of _win
            on error
                set _win_name to ""
            end try
            set _output to _output & "window|" & _win_name & "|enabled|" & (enabled of _win) & "\n"
            set _output to _output & my describe_elements(_win, "  ")
        end repeat
    end tell

    return _output
end tell

on describe_elements(_container, _indent)
    tell application "System Events"
        set _result to ""
        try
            repeat with _elem in (get every UI element of _container)
                try
                    set _role to role of _elem as string
                on error
                    set _role to "unknown"
                end try
                try
                    set _name to name of _elem as string
                on error
                    set _name to ""
                end try
                try
                    set _value to value of _elem as string
                on error
                    set _value to ""
                end try
                try
                    set _enabled to enabled of _elem as string
                on error
                    set _enabled to ""
                end try
                try
                    set _pos to position of _elem as string
                on error
                    set _pos to ""
                end try
                try
                    set _sz to size of _elem as string
                on error
                    set _sz to ""
                end try
                try
                    set _idn to value of attribute "AXIdentifier" of _elem as string
                on error
                    set _idn to ""
                end try

                set _result to _result & _indent & _role & "|" & _name & "|" & _value & "|" & _enabled & "|" & _pos & "|" & _sz & "|" & _idn & "\n"
                set _result to _result & my describe_elements(_elem, _indent & "  ")
            end repeat
        end try
        return _result
    end tell
end describe_elements"#
        .to_string()
    }

    /// Escape a value for an AppleScript string literal.
    fn escape(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// Build an AppleScript element reference from a child-index path.
    ///
    /// `[0, 2, 1]` is the 1st window's 3rd child's 2nd child, written
    /// innermost-first the way AppleScript spells it:
    /// `UI element 2 of UI element 3 of window 1`.
    fn element_reference(path: &[usize]) -> Option<String> {
        let (window, rest) = path.split_first()?;
        let mut reference = format!("window {}", window + 1);
        for index in rest {
            reference = format!("UI element {} of {}", index + 1, reference);
        }
        Some(reference)
    }

    /// Build an AppleScript that performs a native accessibility action on
    /// one element.
    ///
    /// The action rung exists because `click at {x, y}` depends on z-order,
    /// on the window being frontmost, and on nothing covering the point.
    /// `perform action "AXPress"` depends on none of those.
    fn build_action_script(app: &str, path: &[usize], action: &str, value: Option<&str>) -> String {
        let Some(reference) = Self::element_reference(path) else {
            return String::new();
        };
        let body = match action {
            "press" => r#"try
            perform action "AXPress" of _elem
            return "pressed"
        on error
            try
                click _elem
                return "clicked"
            on error _err
                return "error: " & _err
            end try
        end try"#
                .to_string(),
            "focus" => r#"try
            set value of attribute "AXFocused" of _elem to true
            return "focused"
        on error _err
            return "error: " & _err
        end try"#
                .to_string(),
            "set_value" => format!(
                r#"try
            set value of _elem to "{}"
            return "value set"
        on error _err
            return "error: " & _err
        end try"#,
                Self::escape(value.unwrap_or(""))
            ),
            _ => return String::new(),
        };

        format!(
            r#"tell application "System Events"
    tell application process "{}"
        try
            set _elem to {reference}
        on error _err
            return "error: " & _err
        end try
        {body}
    end tell
end tell"#,
            Self::escape(app)
        )
    }

    /// Parse the pipe-delimited, indented output from AppleScript into a tree.
    ///
    /// The AppleScript uses 2-space indentation to represent hierarchy:
    ///
    /// ```text
    /// window|name|enabled|true
    ///   button|OK||true|{100,200}|{80,30}
    ///     static text|Click me||||
    /// ```
    ///
    /// Returns a `Vec<UiElement>` of top-level windows, each with nested children.
    fn parse_tree_output(output: &str) -> Vec<UiElement> {
        // Pre-parse: skip empty / app lines, compute depth from leading spaces.
        struct Entry {
            depth: usize,
            role: String,
            name: String,
            value: Option<String>,
            enabled: Option<bool>,
            position: Option<String>,
            size: Option<String>,
            identifier: Option<String>,
        }

        let entries: Vec<Entry> = output
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    return None;
                }
                let content = trimmed.trim_start();
                let leading = trimmed.len() - content.len();
                // Skip the "app|Safari" metadata line.
                if content.starts_with("app|") {
                    return None;
                }
                let parts: Vec<&str> = content.split('|').collect();
                if parts.len() < 2 {
                    return None;
                }
                Some(Entry {
                    depth: leading / 2,
                    role: parts[0].to_string(),
                    name: parts[1].to_string(),
                    value: parts
                        .get(2)
                        .map(|s| s.to_string())
                        .filter(|s| !s.is_empty()),
                    enabled: parts.get(3).and_then(|s| match *s {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    }),
                    position: parts
                        .get(4)
                        .map(|s| s.to_string())
                        .filter(|s| !s.is_empty()),
                    size: parts
                        .get(5)
                        .map(|s| s.to_string())
                        .filter(|s| !s.is_empty()),
                    identifier: parts
                        .get(6)
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                })
            })
            .collect();

        // Recursively build tree: process entries at `depth`, recursively
        // collect children at `depth + 1`.
        fn build(entries: &[Entry], start: usize, depth: usize) -> (Vec<UiElement>, usize) {
            let mut elements = Vec::new();
            let mut i = start;
            while i < entries.len() && entries[i].depth >= depth {
                if entries[i].depth > depth {
                    // Skip child entries — they'll be consumed by the recursive call below.
                    break;
                }
                let (children, next) = build(entries, i + 1, depth + 1);
                elements.push(UiElement {
                    role: entries[i].role.clone(),
                    name: entries[i].name.clone(),
                    value: entries[i].value.clone(),
                    enabled: entries[i].enabled,
                    position: entries[i].position.clone(),
                    size: entries[i].size.clone(),
                    identifier: entries[i].identifier.clone(),
                    children,
                });
                i = next;
            }
            (elements, i)
        }

        let (result, _) = build(&entries, 0, 0);
        result
    }

    /// Run a native accessibility action against an element path.
    async fn run_action(action: &str, args: &Value) -> crate::Result<ToolExecutionResult> {
        let Some(app) = args["app"].as_str().filter(|a| !a.is_empty()) else {
            return Ok(ToolExecutionResult::error("an accessibility action needs an app name"));
        };
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

        let script = Self::build_action_script(app, &path, action, args["value"].as_str());
        if script.is_empty() {
            return Ok(ToolExecutionResult::error(format!(
                "unsupported accessibility action '{action}'"
            )));
        }

        let timeout_secs = args["timeout"].as_u64().unwrap_or(10);
        let as_result = AppleScriptTool::execute_script(&script, timeout_secs).await;
        let output = as_result.output.trim().to_string();
        if as_result.success && !output.starts_with("error:") {
            Ok(ToolExecutionResult::success(&output).with_data(serde_json::json!({
                "success": true,
                "action": action,
                "app": app,
                "path": path,
                "result": output,
            })))
        } else {
            let error = as_result
                .error
                .clone()
                .filter(|e| !e.is_empty())
                .unwrap_or(output);
            Ok(ToolExecutionResult::error(error.clone()).with_data(serde_json::json!({
                "success": false,
                "action": action,
                "app": app,
                "path": path,
                "error": error,
            })))
        }
    }
}

#[async_trait]
impl Tool for AccessibilityTool {
    fn name(&self) -> &str {
        "macos_accessibility"
    }

    fn description(&self) -> &str {
        "Query the macOS accessibility/UI tree for a specific application or the frontmost \
         application. Returns a structured tree of windows, buttons, text fields, and other UI \
         elements. This is the primary perception mechanism for desktop control — use it before \
         taking screenshots whenever possible."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Query UI tree via Accessibility API",
            serde_json::json!({
                "app": {
                    "type": "string",
                    "description": "Application name to inspect (e.g. Safari, Finder). If omitted, inspects the frontmost application."
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Maximum recursion depth (AppleScript may time out for deep trees)",
                    "default": 3
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
        // Early-exit with a clear error if accessibility permission is missing.
        if !super::permissions::has_accessibility_permission() {
            let guide = super::permissions::accessibility_permission_guide();
            let err_msg = format!(
                "macOS Accessibility permission not granted. Please grant it in System Settings → \
                 Privacy & Security → Accessibility.\n\n{}",
                guide
            );
            warn!("Accessibility tool blocked: {}", err_msg);
            return Ok(ToolExecutionResult::error(err_msg.clone()).with_data(serde_json::json!({
                "success": false,
                "error": err_msg,
                "needs_permission": true,
            })));
        }

        // Native accessibility actions (the ladder's `ax` rung) share this
        // tool because they share its permission check and its process
        // addressing.
        if let Some(action) = args["action"].as_str().filter(|a| *a != "tree") {
            return Self::run_action(action, &args).await;
        }

        let script = if let Some(app) = args["app"].as_str() {
            info!("Querying accessibility tree for app: {}", app);
            Self::build_ui_tree_script(app)
        } else {
            info!("Querying accessibility tree for frontmost app");
            Self::build_frontmost_script()
        };

        let as_result =
            AppleScriptTool::execute_script(&script, args["timeout"].as_u64().unwrap_or(15)).await;

        let mut result = AccessibilityResult {
            success: as_result.success,
            app: None,
            elements: Vec::new(),
            raw_output: Some(as_result.output.clone()),
            error: as_result.error.clone(),
        };

        if as_result.success {
            result.app = as_result
                .output
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("app|").map(|s| s.to_string()));
            result.elements = Self::parse_tree_output(&as_result.output);
        } else {
            warn!(
                "Accessibility query failed: {}",
                as_result.error.as_deref().unwrap_or("unknown error")
            );
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
        cfg!(target_os = "macos")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accessibility_tool_creation() {
        let tool = AccessibilityTool::new();
        assert_eq!(tool.name(), "macos_accessibility");
        assert!(tool.description().contains("UI tree"));
    }

    #[test]
    fn test_parse_tree_output() {
        // Note: explicit "\n" + spaces — Rust's `\` line continuation strips
        // leading whitespace, which would destroy the indentation the parser
        // relies on.
        let output = "app|Safari\nwindow|GitHub|enabled|true\n  button|Reload||true||\n  text field|Search||false|{100, 200}|{300, 30}\n    static text|https://github.com|https://github.com||";
        let elements = AccessibilityTool::parse_tree_output(output);
        // app line is skipped; top-level should have 1 window
        assert_eq!(elements.len(), 1, "should have 1 top-level window");
        assert_eq!(elements[0].role, "window");
        // Window has 2 children: button + text field
        assert_eq!(elements[0].children.len(), 2);
        assert_eq!(elements[0].children[0].role, "button");
        assert_eq!(elements[0].children[1].role, "text field");
        assert_eq!(elements[0].children[1].position.as_deref(), Some("{100, 200}"));
        // Text field has 1 child: static text
        assert_eq!(elements[0].children[1].children.len(), 1);
        assert_eq!(elements[0].children[1].children[0].role, "static text");
    }
}
