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
    /// The element's declared `AXActions`. `None` means the read did not say;
    /// `Some([])` means it said "none".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    /// How many children the element has in the application, as counted
    /// before the depth cut. A count larger than `children.len()` is the
    /// machine-readable form of "this subtree was cut".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<u32>,
    /// Whether the element holds keyboard focus. `None` means the script
    /// could not read it; the adapter turns that into `Tri::Unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    /// Whether the walk stopped here instead of at the bottom of the tree.
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
    ///
    /// Each line carries the element's own declared actions
    /// (`name of every action`, i.e. `AXActions`) as the last column. The
    /// action rung is gated on that list: an element that publishes its
    /// actions and does not include the one about to be performed is skipped,
    /// because a responder can answer `kAXErrorSuccess` to an action it never
    /// published and never ran.
    ///
    /// `max_depth` is honoured here rather than ignored: a deep tree used to
    /// recurse without bound, time out, and be reported as a missing
    /// accessibility permission.
    fn build_ui_tree_script(app_name: &str, max_depth: u32) -> String {
        Self::tree_script(&format!(
            r#"tell application "System Events"
    tell application process "{}"
        set _output to "app|{}" & "\n"
        set _indent to "  "

        repeat with _win in (get every window)
            set _output to _output & "window|" & (name of _win) & "|enabled|" & (enabled of _win) & "|\n"
            set _output to _output & my describe_elements(_win, _indent, 1, {max_depth})
        end repeat

        return _output
    end tell
end tell"#,
            Self::escape(app_name),
            app_name
        ))
    }

    /// Build the script for "whatever is frontmost".
    ///
    /// It produces exactly the same columns as [`Self::build_ui_tree_script`]
    /// — including the declared actions — because one parser reads both, and
    /// a column the script omits is a column the parser reports as unknown.
    ///
    /// `max_depth` is the caller's, not a constant: the schema declares the
    /// parameter for both branches, so honouring it on only one of them would
    /// be a lie the caller cannot see.
    fn build_frontmost_script(max_depth: u32) -> String {
        Self::tree_script(&format!(
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
            try
                set _win_enabled to enabled of _win as string
            on error
                set _win_enabled to ""
            end try
            set _output to _output & "window|" & _win_name & "|enabled|" & _win_enabled & "|\n"
            set _output to _output & my describe_elements(_win, "  ", 1, {max_depth})
        end repeat
    end tell

    return _output
end tell"#
        ))
    }

    /// One tree walk, shared by both entry points.
    ///
    /// The two callers differ only in how they name the process; the columns
    /// and the truncation rule must not drift apart, so the walk itself is
    /// assembled once.
    ///
    /// Columns: `role|name|value|enabled|position|size|identifier|actions|
    /// children_count|focused|state`.
    /// Every column after `name` may be empty, which the parser reads as
    /// "the script did not say" rather than as a default.
    ///
    /// A node past `max_depth` is still reported — with its role, name,
    /// geometry and **child count** — and marked `cut` on a line of its own.
    /// That is what makes "the tree is incomplete" a fact the caller can act
    /// on, instead of a depth limit the model cannot see.
    fn tree_script(header: &str) -> String {
        format!(
            r#"{header}

on describe_elements(_container, _indent, _depth, _max_depth)
    tell application "System Events"
        set _result to ""
        try
            set _elems to (get every UI element of _container)
        on error
            set _elems to {{}}
        end try

        if _depth > _max_depth then
            -- Below the cut: report the element, its geometry and how many
            -- children remain unread, then stop. The count is the evidence
            -- that something was left out.
            repeat with _elem in _elems
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
                    set _kids to (count of (get every UI element of _elem)) as string
                on error
                    set _kids to ""
                end try
                -- Columns: role|name|value|enabled|position|size|identifier|
                -- actions|children_count|focused|state. A cut node's value,
                -- enabled, identifier, actions and focus were not read.
                set _result to _result & _indent & _role & "|" & _name & "||||" & _pos & "|" & _sz & "||||" & _kids & "|cut" & "\n"
            end repeat
            return _result
        end if

        try
            repeat with _elem in _elems
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
                try
                    set _acts to ""
                    repeat with _act in (get name of every action of _elem)
                        if _acts is not "" then set _acts to _acts & ","
                        set _acts to _acts & (_act as string)
                    end repeat
                on error
                    set _acts to ""
                end try
                try
                    set _kids to (count of (get every UI element of _elem)) as string
                on error
                    set _kids to ""
                end try
                -- Focus is the one fact that decides where a bare `type` or
                -- `key_press` lands, and it was never read.
                try
                    set _focused to (value of attribute "AXFocused" of _elem) as string
                on error
                    set _focused to ""
                end try

                set _result to _result & _indent & _role & "|" & _name & "|" & _value & "|" & _enabled & "|" & _pos & "|" & _sz & "|" & _idn & "|" & _acts & "|" & _kids & "|" & _focused & "|ok" & "\n"
                set _result to _result & my describe_elements(_elem, _indent & "  ", _depth + 1, _max_depth)
            end repeat
        end try
        return _result
    end tell
end describe_elements"#
        )
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
            actions: Option<Vec<String>>,
            /// The `children_count` column: the element says how many
            /// children it has, before the walk decided how many to read.
            children_count: Option<u32>,
            /// The `focused` column: whether this element holds keyboard
            /// focus. `None` where the script could not read it.
            focused: Option<bool>,
            /// The `ok` / `cut` column: this node's own children were or
            /// were not fully read.
            truncated: bool,
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
                // Split from both ends: the trailing columns are positional
                // and known, while the name/value columns may themselves
                // contain the separator. `role|name|value|enabled|position|
                // size|identifier|actions|children_count|state`.
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
                    // An empty column means the script could not read the
                    // action list; a non-empty one is the element's answer.
                    actions: parts
                        .get(7)
                        .map(|s| {
                            s.split(',')
                                .map(str::trim)
                                .filter(|a| !a.is_empty())
                                .map(String::from)
                                .collect::<Vec<_>>()
                        })
                        .filter(|a: &Vec<String>| !a.is_empty()),
                    // A column the script did not fill is "unknown", never
                    // "zero children": only a number is a count.
                    children_count: parts.get(8).and_then(|s| s.trim().parse::<u32>().ok()),
                    focused: parts.get(9).and_then(|s| match s.trim() {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    }),
                    truncated: parts.get(10).map(|s| s.trim() == "cut").unwrap_or(false),
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
                    actions: entries[i].actions.clone(),
                    focused: entries[i].focused,
                    // A parent is incomplete when any child was cut, even if
                    // the parent's own count is complete — the missing
                    // evidence is somewhere below it.
                    children_count: entries[i].children_count,
                    truncated: entries[i].truncated || children.iter().any(|c| c.truncated),
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

        let max_depth = args["max_depth"]
            .as_u64()
            .map(|d| d.clamp(1, 16) as u32)
            .unwrap_or(3);
        let script = if let Some(app) = args["app"].as_str() {
            info!("Querying accessibility tree for app: {}", app);
            Self::build_ui_tree_script(app, max_depth)
        } else {
            info!("Querying accessibility tree for frontmost app");
            Self::build_frontmost_script(max_depth)
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

    /// The tree script's column order, for tests that write raw output.
    fn line(indent: usize, fields: &[&str]) -> String {
        let mut all = fields.to_vec();
        // role|name|value|enabled|position|size|identifier|actions|count|focused|state
        all.resize(11, "");
        format!("{}{}", "  ".repeat(indent), all.join("|"))
    }

    /// A node below the depth limit is reported with its geometry and the
    /// number of children nobody read, and is marked `cut`. Without that the
    /// depth limit is invisible: the caller sees a leaf and believes it.
    #[test]
    fn a_cut_node_says_how_much_was_left_unread() {
        let output = [
            "app|Safari".to_string(),
            line(
                0,
                &[
                    "window", "GitHub", "", "true", "", "", "", "", "2", "", "ok",
                ],
            ),
            line(
                1,
                &[
                    "button", "Reload", "", "true", "{1, 2}", "{3, 4}", "", "", "1", "", "ok",
                ],
            ),
            line(2, &["group", "Toolbar", "", "", "", "", "", "", "2", "", "cut"]),
        ]
        .join("\n");
        let elements = AccessibilityTool::parse_tree_output(&output);
        let window = &elements[0];
        assert_eq!(window.children_count, Some(2));

        let group = &window.children[0].children[0];
        assert_eq!(group.role, "group");
        assert_eq!(group.children_count, Some(2));
        assert!(group.truncated);
        assert!(group.children.is_empty(), "a cut node's children were not read");
    }

    /// The cut belongs to the node the walk stopped at, but it is the whole
    /// tree that is incomplete — so every ancestor reports it.
    #[test]
    fn a_cut_deep_in_the_tree_makes_its_ancestors_incomplete() {
        let output = [
            "app|Safari".to_string(),
            line(
                0,
                &[
                    "window", "GitHub", "", "true", "", "", "", "", "1", "", "ok",
                ],
            ),
            line(1, &["button", "Reload", "", "", "", "", "", "", "1", "", "ok"]),
            line(2, &["group", "Toolbar", "", "", "", "", "", "", "4", "", "cut"]),
        ]
        .join("\n");
        let elements = AccessibilityTool::parse_tree_output(&output);
        assert!(elements[0].truncated, "the root inherits the cut below it");
        assert!(elements[0].children[0].truncated);
    }

    /// A complete tree says so: the count matches what was read, and nothing
    /// is flagged.
    #[test]
    fn a_fully_read_tree_carries_no_truncation() {
        let output = [
            "app|Safari".to_string(),
            line(
                0,
                &[
                    "window", "GitHub", "", "true", "", "", "", "", "1", "", "ok",
                ],
            ),
            line(1, &["button", "Reload", "", "", "", "", "", "", "0", "", "ok"]),
        ]
        .join("\n");
        let elements = AccessibilityTool::parse_tree_output(&output);
        assert_eq!(elements[0].children_count, Some(1));
        assert_eq!(elements[0].children.len(), 1);
        assert!(!elements[0].truncated);
        assert!(!elements[0].children[0].truncated);
    }

    /// Focus is read, and an empty column is "unknown" rather than "not
    /// focused" — the same three-state rule as `enabled`.
    #[test]
    fn a_focused_field_is_reported_and_an_unread_one_is_unknown() {
        let output = [
            "app|Safari".to_string(),
            line(
                0,
                &[
                    "window", "GitHub", "", "true", "", "", "", "", "1", "", "ok",
                ],
            ),
            line(
                1,
                &[
                    "text field",
                    "Search",
                    "",
                    "true",
                    "",
                    "",
                    "",
                    "",
                    "0",
                    "true",
                    "ok",
                ],
            ),
        ]
        .join(
            "
",
        );
        let elements = AccessibilityTool::parse_tree_output(&output);
        assert_eq!(elements[0].children[0].focused, Some(true));

        let unread = AccessibilityTool::parse_tree_output(&line(
            0,
            &[
                "window", "GitHub", "", "true", "", "", "", "", "0", "", "ok",
            ],
        ));
        assert_eq!(unread[0].focused, None, "an unread column is not 'not focused'");
    }

    /// The script must actually produce those two columns, and must not
    /// recurse past the limit — the limit is what it reports, not what it
    /// hides.
    #[test]
    fn the_script_counts_children_and_marks_the_cut() {
        let script = AccessibilityTool::build_ui_tree_script("Safari", 3);
        assert!(script.contains("count of (get every UI element of _elem)"), "{script}");
        assert!(script.contains("AXFocused"), "focus is read: {script}");
        assert!(script.contains("|cut"), "{script}");
        assert!(script.contains("|ok"), "{script}");
        assert!(script.contains("1, 3"), "the depth limit is substituted in: {script}");
        // Below the cut the walk returns before recursing.
        let cut = script
            .find("if _depth > _max_depth then")
            .expect("cut branch");
        let recurse = script
            .find("my describe_elements(_elem")
            .expect("recursion");
        assert!(cut < recurse);
    }

    /// The frontmost branch honours the caller's depth too. The schema offers
    /// `max_depth` whether or not `app` is named, so a constant here would be
    /// a parameter that only sometimes works.
    #[test]
    fn the_frontmost_script_honours_the_requested_depth() {
        let script = AccessibilityTool::build_frontmost_script(6);
        assert!(script.contains("my describe_elements(_win, \"  \", 1, 6)"), "{script}");
        assert!(!script.contains("frontmost of"), "reading must not take the foreground");
    }

    /// Reading the tree addresses the process by name; it must never activate
    /// it. `set frontmost to true` used to be in this script, and it stole the
    /// screen from whoever was using it.
    #[test]
    fn reading_the_tree_does_not_activate_the_app() {
        for script in [
            AccessibilityTool::build_ui_tree_script("Safari", 3),
            AccessibilityTool::build_frontmost_script(3),
        ] {
            assert!(
                !script.contains("set frontmost to true") && !script.contains("\nactivate"),
                "{script}"
            );
            // One addresses the process by name, the other through System
            // Events' frontmost process; neither activates anything.
            assert!(script.contains("tell application \"System Events\""), "{script}");
        }
    }
}
