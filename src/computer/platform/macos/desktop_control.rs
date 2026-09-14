//! macOS Desktop control tool — orchestrate accessibility + screenshot hybrid.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use super::accessibility::AccessibilityTool;
use super::screenshot::ScreenshotTool;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Perception mode for desktop control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerceptionMode {
    /// Structured UI tree only.
    AccessibilityOnly,
    /// Screenshot only.
    ScreenshotOnly,
    /// UI tree + screenshot for visual validation.
    Hybrid,
}

/// Action to perform on the desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopAction {
    /// Inspect current UI state.
    Inspect,
    /// Click a UI element by role + name.
    Click {
        app: Option<String>,
        role: String,
        name: String,
    },
    /// Type text into a text field.
    /// When `app` and `field` are omitted, uses clipboard paste at
    /// the current cursor position.
    Type {
        app: Option<String>,
        field: Option<String>,
        text: String,
    },
    /// Press a keyboard shortcut.
    KeyShortcut {
        app: Option<String>,
        keys: Vec<String>,
    },
}

/// Result of a desktop control operation.
#[derive(Debug, Clone, Serialize)]
pub struct DesktopControlResult {
    pub success: bool,
    pub mode: String,
    pub action: String,
    pub accessibility: Option<serde_json::Value>,
    pub screenshot: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// High-level desktop control tool for macOS.
///
/// Implements the hybrid perception model:
/// 1. Query accessibility tree (primary)
/// 2. Take screenshot when visual validation is needed
/// 3. Execute actions via AppleScript
#[derive(Debug)]
pub struct DesktopControlTool;

impl Default for DesktopControlTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopControlTool {
    pub fn new() -> Self {
        Self
    }

    fn build_click_script(app: &str, role: &str, name: &str) -> String {
        format!(
            r#"tell application "{}" to activate
delay 0.2
tell application "System Events"
    tell application process "{}"
        click (first UI element whose role is "{}" and name is "{}")
    end tell
end tell"#,
            app, app, role, name
        )
    }

    fn build_type_script(app: &str, field: &str, text: &str) -> String {
        format!(
            r#"tell application "{}" to activate
delay 0.2
tell application "System Events"
    tell application process "{}"
        set _field to first text field whose name is "{}"
        set focused of _field to true
        set value of _field to "{}"
    end tell
end tell"#,
            app,
            app,
            field,
            text.replace('"', "\\\"")
        )
    }

    /// Type text at the current cursor position using clipboard paste.
    /// Saves and restores the clipboard to minimise side effects.
    fn build_type_at_cursor_script(text: &str) -> String {
        let escaped = text.replace('"', "\\\"");
        format!(
            r#"set _clipboard to (the clipboard as string)
set the clipboard to "{}"
tell application "System Events"
    keystroke "v" using command down
end tell
delay 0.1
set the clipboard to _clipboard"#,
            escaped
        )
    }

    fn build_keystroke_script(app: Option<&str>, keys: &[String]) -> String {
        let app_block = if let Some(name) = app {
            format!("tell application \"{}\" to activate\ndelay 0.2\n", name)
        } else {
            String::new()
        };

        let key_lines: Vec<String> = keys
            .iter()
            .map(|k| match k.as_str() {
                "cmd" | "command" => "key down command".to_string(),
                "shift" => "key down shift".to_string(),
                "option" | "alt" => "key down option".to_string(),
                "ctrl" | "control" => "key down control".to_string(),
                _ => format!("keystroke \"{}\"", k.replace('"', "\\\"")),
            })
            .collect();

        let key_up_lines: Vec<String> = keys
            .iter()
            .filter_map(|k| match k.as_str() {
                "cmd" | "command" => Some("key up command".to_string()),
                "shift" => Some("key up shift".to_string()),
                "option" | "alt" => Some("key up option".to_string()),
                "ctrl" | "control" => Some("key up control".to_string()),
                _ => None,
            })
            .collect();

        format!(
            r#"{}tell application "System Events"
    {}
    {}
end tell"#,
            app_block,
            key_lines.join("\n    "),
            key_up_lines.join("\n    ")
        )
    }
}

#[async_trait]
impl Tool for DesktopControlTool {
    fn name(&self) -> &str {
        "macos_desktop_control"
    }

    fn description(&self) -> &str {
        "Control macOS desktop applications using a hybrid model: query the accessibility UI tree \
         first, then screenshot if needed, and execute actions via AppleScript. Use for opening \
         apps, clicking buttons, filling forms, pressing shortcuts, or inspecting the current GUI \
         state. Input goes to the live session: it moves the real pointer and types into whatever \
         holds focus, so on a desktop someone is using, say what you intend and ask before acting."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Control macOS desktop",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action to perform",
                    "enum": ["inspect", "click", "type", "key_shortcut", "close_window"]
                },
                "mode": {
                    "type": "string",
                    "description": "Perception mode (default: hybrid)",
                    "enum": ["accessibility_only", "screenshot_only", "hybrid"],
                    "default": "hybrid"
                },
                "app": {
                    "type": "string",
                    "description": "Target application name (e.g. Safari, Finder, Slack)"
                },
                "role": {
                    "type": "string",
                    "description": "UI element role for click action (e.g. button, menu item)"
                },
                "name": {
                    "type": "string",
                    "description": "UI element name for click action"
                },
                "field": {
                    "type": "string",
                    "description": "Text field name for type action"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type"
                },
                "keys": {
                    "type": "array",
                    "description": "Keys for keyboard shortcut (e.g. [\"cmd\", \"t\"] for Cmd+T)",
                    "items": { "type": "string" }
                }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // One pointer and one focus: two conversations must not drive this
        // display at once. See `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::DESKTOP)
            .await;
        let action_str = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'action' argument".to_string())
        })?;

        let mode = match args["mode"].as_str().unwrap_or("hybrid") {
            "accessibility_only" => PerceptionMode::AccessibilityOnly,
            "screenshot_only" => PerceptionMode::ScreenshotOnly,
            _ => PerceptionMode::Hybrid,
        };

        info!("desktop_control: action={}, mode={:?}", action_str, mode);

        let mut result = DesktopControlResult {
            success: true,
            mode: format!("{:?}", mode),
            action: action_str.to_string(),
            accessibility: None,
            screenshot: None,
            error: None,
        };

        // ── Perception step ────────────────────────────────────────────────
        match mode {
            PerceptionMode::AccessibilityOnly | PerceptionMode::Hybrid => {
                let acc_args = if let Some(app) = args["app"].as_str() {
                    serde_json::json!({"app": app})
                } else {
                    serde_json::json!({})
                };

                let acc_tool = AccessibilityTool::new();
                match acc_tool.execute(acc_args, context).await {
                    Ok(exec_result) => {
                        if let Some(data) = exec_result.data {
                            result.accessibility = Some(data);
                        }
                        if !exec_result.success {
                            let err = exec_result
                                .error
                                .unwrap_or_else(|| "Accessibility query failed".to_string());
                            warn!("Accessibility query failed: {}", err);
                            result.error = Some(err);
                        }
                    }
                    Err(e) => {
                        warn!("Accessibility query failed: {}", e);
                        result.error = Some(format!("Accessibility query failed: {}", e));
                    }
                }
            }
            PerceptionMode::ScreenshotOnly => {}
        }

        if mode == PerceptionMode::Hybrid || mode == PerceptionMode::ScreenshotOnly {
            let shot_tool = ScreenshotTool::new();
            match shot_tool.execute(serde_json::json!({}), context).await {
                Ok(exec_result) => {
                    if let Some(data) = exec_result.data {
                        result.screenshot = Some(data);
                    }
                }
                Err(e) => {
                    warn!("Screenshot failed: {}", e);
                }
            }
        }

        // ── Action step ────────────────────────────────────────────────────
        let action_result = match action_str {
            "inspect" => Ok(()),
            "click" => {
                let app = args["app"].as_str().unwrap_or("System Events");
                let role = args["role"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'role' for click action".to_string(),
                    )
                })?;
                let name = args["name"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'name' for click action".to_string(),
                    )
                })?;

                let script = Self::build_click_script(app, role, name);
                let as_result =
                    super::applescript::AppleScriptTool::execute_script(&script, 15).await;
                if as_result.success {
                    Ok(())
                } else {
                    Err(crate::error::SyscityError::Validation(
                        as_result
                            .error
                            .unwrap_or_else(|| "Click failed".to_string()),
                    ))
                }
            }
            "type" => {
                let text = args["text"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'text' for type action".to_string(),
                    )
                })?;

                // When app + field are provided, target a specific text field.
                // Otherwise fall back to clipboard-paste at the cursor position.
                let script = if let (Some(app), Some(field)) =
                    (args["app"].as_str(), args["field"].as_str())
                {
                    Self::build_type_script(app, field, text)
                } else {
                    Self::build_type_at_cursor_script(text)
                };

                let as_result =
                    super::applescript::AppleScriptTool::execute_script(&script, 15).await;
                if as_result.success {
                    Ok(())
                } else {
                    Err(crate::error::SyscityError::Validation(
                        as_result.error.unwrap_or_else(|| "Type failed".to_string()),
                    ))
                }
            }
            "key_shortcut" => {
                let app = args["app"].as_str();
                let keys: Vec<String> = args["keys"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                if keys.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "Missing 'keys' for key_shortcut action".to_string(),
                    ));
                }

                let script = Self::build_keystroke_script(app, &keys);
                let as_result =
                    super::applescript::AppleScriptTool::execute_script(&script, 15).await;
                if as_result.success {
                    Ok(())
                } else {
                    Err(crate::error::SyscityError::Validation(
                        as_result
                            .error
                            .unwrap_or_else(|| "Keystroke failed".to_string()),
                    ))
                }
            }
            "close_window" => {
                let app = args["app"].as_str().unwrap_or("System Events");
                let script = format!(
                    r#"tell application "{}" to activate
delay 0.2
tell application "System Events"
    keystroke "w" using command down
end tell"#,
                    app
                );
                let as_result =
                    super::applescript::AppleScriptTool::execute_script(&script, 15).await;
                if as_result.success {
                    Ok(())
                } else {
                    Err(crate::error::SyscityError::Validation(
                        as_result
                            .error
                            .unwrap_or_else(|| "Close window failed".to_string()),
                    ))
                }
            }
            _ => Err(crate::error::SyscityError::Validation(format!(
                "Unknown action: {}",
                action_str
            ))),
        };

        if let Err(e) = action_result {
            result.success = false;
            result.error = Some(e.to_string());
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
    fn test_desktop_control_tool_creation() {
        let tool = DesktopControlTool::new();
        assert_eq!(tool.name(), "macos_desktop_control");
        assert!(tool.description().contains("hybrid"));
    }

    #[test]
    fn test_build_keystroke_script() {
        let script = DesktopControlTool::build_keystroke_script(
            Some("Safari"),
            &["cmd".to_string(), "t".to_string()],
        );
        assert!(script.contains("tell application \"Safari\""));
        assert!(script.contains("key down command"));
        assert!(script.contains("keystroke \"t\""));
        assert!(script.contains("key up command"));
    }
}
