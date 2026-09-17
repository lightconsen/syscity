//! Tool wrapper around [`ComputerAdapter`] exposing desktop automation
//! operations as standard Tool trait implementations.
//!
//! The LLM calls `computer` with an `action` parameter to perform screenshots,
//! clicks, typing, key presses, window management, and other desktop ops.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

use crate::computer::vision::{is_screen_mutating_action, ScreenState};
use crate::computer::{
    ActionResult, ClickTarget, ComputerAdapter, ComputerError, DesktopAction, MouseButton, Point,
    Rect, ScrollDirection,
};
use crate::tools::{
    approval::RiskLevel, create_schema, sdk::ToolCapabilities, Tool, ToolContext,
    ToolExecutionResult,
};

/// Tool that exposes all [`ComputerAdapter`] operations to the LLM.
///
/// Uses the `action` enum pattern (like [`TimeTool`]) — each action maps to a
/// [`DesktopAction`] variant.  The tool reports `is_available=false` when no
/// adapter is configured, preventing the LLM from calling it.
pub struct ComputerTool {
    adapter: Option<Arc<dyn ComputerAdapter>>,
}

impl ComputerTool {
    pub fn new(adapter: Option<Arc<dyn ComputerAdapter>>) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "computer"
    }

    fn description(&self) -> &str {
        r#"Perform desktop automation operations: capture screenshots, move and click the mouse, type text, press keyboard shortcuts, scroll, drag, manage windows, launch applications, read UI accessibility trees, query system status, list/kill processes, test network connectivity, list firewall rules, manage file watchers, and browse files.

Use the `action` parameter to choose the operation. Each action uses its own set of parameters — see the action enum descriptions for details.

Mouse and keyboard actions go to the live session: they move the real pointer and type into whatever holds focus. On a desktop someone is using, say what you intend and ask before acting.

Common workflows:
- "screenshot" — capture the screen (optionally a region)
- "click" — click at coordinates or on a UI element
- "type" — type text at the current cursor position
- "key_press" — press keyboard shortcuts like ["ctrl","c"]
- "read_ui_tree" — inspect the accessibility tree of the active window"#
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Desktop automation operations",
            json!({
                "action": {
                    "type": "string",
                    "enum": [
                        "screenshot", "click", "double_click", "type", "key_press",
                        "scroll", "drag", "read_ui_tree", "launch_app",
                        "activate_window", "close_window", "wait",
                        "clipboard_get", "clipboard_set", "get_system_status",
                        "list_processes", "kill_process",
                        "restart_process", "set_process_priority",
                        "list_windows", "get_window_geometry", "move_window",
                        "resize_window", "minimize_window", "maximize_window"
                    ],
                    "description": "The desktop operation to perform"
                },
                // ── Common positional parameters ──────────────────────────
                "x": { "type": "integer", "description": "X coordinate (for click, double_click, scroll, drag_from, drag_to)" },
                "y": { "type": "integer", "description": "Y coordinate" },
                "text": { "type": "string", "description": "Text to type, or clipboard content to set" },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Key names to press (e.g. [\"ctrl\",\"c\"], [\"cmd\",\"space\"], [\"enter\"])"
                },
                "button": {
                    "type": "string",
                    "enum": ["left", "middle", "right"],
                    "description": "Mouse button for click/double_click"
                },
                // ── Region / scroll ───────────────────────────────────────
                "region_x": { "type": "integer", "description": "Screenshot region left coordinate" },
                "region_y": { "type": "integer", "description": "Screenshot region top coordinate" },
                "region_width": { "type": "integer", "description": "Screenshot region width" },
                "region_height": { "type": "integer", "description": "Screenshot region height" },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction"
                },
                "amount": { "type": "integer", "description": "Scroll amount (clicks or pixels)" },
                // ── Window / app ──────────────────────────────────────────
                "title_pattern": { "type": "string", "description": "Window title pattern to match (activate_window, close_window)" },
                "app_name": { "type": "string", "description": "Application name to launch" },
                "app_args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command-line arguments for the application"
                },
                "wait_for_ready": { "type": "boolean", "description": "Wait for the app window to appear before returning" },
                // ── Process ───────────────────────────────────────────────
                "filter": { "type": "string", "description": "Process name filter (list_processes)" },
                "pid": { "type": "integer", "description": "Process ID (kill_process, restart_process, set_process_priority)" },
                "name": { "type": "string", "description": "Process name (kill_process, restart_process, set_process_priority)" },
                "force": { "type": "boolean", "description": "Force kill (kill_process, restart_process)" },
                "priority": { "type": "integer", "description": "Priority value (set_process_priority): Unix nice -20..19, Windows 0..5" },
                // ── Wait ──────────────────────────────────────────────────
                "milliseconds": { "type": "integer", "description": "Duration to wait in milliseconds" },
                // ── Drag sub-params ───────────────────────────────────────
                "from_x": { "type": "integer", "description": "Drag start X" },
                "from_y": { "type": "integer", "description": "Drag start Y" },
                "to_x": { "type": "integer", "description": "Drag end X" },
                "to_y": { "type": "integer", "description": "Drag end Y" },
                // ── Window management ───────────────────────────────────
                "width": { "type": "integer", "description": "Target width (resize_window)" },
                "height": { "type": "integer", "description": "Target height (resize_window)" },
            }),
            vec!["action"],
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: RiskLevel::Medium,
            categories: vec!["computer".to_string(), "desktop".to_string()],
            // UI actions act on whatever is on screen *now*: repeating one
            // clicks a second time, which is rarely what a retry means.
            idempotent: false,
            compensation: None,
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        self.adapter.is_some()
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let adapter = self.adapter.as_ref().ok_or_else(|| {
            crate::error::SyscityError::Unsupported(
                "Computer adapter is not configured".to_string(),
            )
        })?;

        let _t_start = std::time::Instant::now();
        let action = args["action"]
            .as_str()
            .ok_or_else(|| crate::error::SyscityError::Validation("Missing action".to_string()))?;

        let desktop_action = action_to_desktop_action(action, &args)?;

        // Transparent verification: for screen-mutating actions, capture a
        // lightweight pre-snapshot so we can diff against the post-state and
        // tell the LLM whether the action had a visible effect.
        let verify = is_screen_mutating_action(&desktop_action);
        let pre_state = if verify {
            ScreenState::capture_light(adapter.as_ref()).await.ok()
        } else {
            None
        };

        let adapter_start = std::time::Instant::now();
        let result = adapter
            .execute(desktop_action)
            .await
            .map_err(to_syscity_err)?;
        let adapter_elapsed = adapter_start.elapsed();
        info!(
            "[ComputerTool] action={} adapter.execute() took {:?} (total so far: {:?})",
            action,
            adapter_elapsed,
            _t_start.elapsed()
        );

        let mut tool_result = action_result_to_tool_result(result).await;

        if let Some(pre) = pre_state {
            // Brief settle delay so the UI has time to react to the action.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            match ScreenState::capture_light(adapter.as_ref()).await {
                Ok(post) => {
                    let diff = pre.diff(&post);
                    tool_result
                        .output
                        .push_str(&format!("\n\n[verification] {}", diff.summary()));
                    let diff_json = serde_json::to_value(&diff).unwrap_or_default();
                    let data = tool_result.data.get_or_insert_with(|| json!({}));
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert("verification".to_string(), diff_json);
                    }
                }
                Err(e) => {
                    tracing::warn!("post-action verification capture failed: {}", e);
                }
            }
        }

        info!("[ComputerTool] action={} total execute() took {:?}", action, _t_start.elapsed());

        Ok(tool_result)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn action_to_desktop_action(action: &str, args: &Value) -> crate::Result<DesktopAction> {
    match action {
        "screenshot" => {
            let region = parse_region(args);
            Ok(DesktopAction::Screenshot { region })
        }
        "click" => {
            let target = parse_click_target(args)?;
            let button = parse_button(args);
            Ok(DesktopAction::Click { target, button })
        }
        "double_click" => {
            let target = parse_click_target(args)?;
            let button = parse_button(args);
            Ok(DesktopAction::DoubleClick { target, button })
        }
        "type" => {
            let text = args["text"]
                .as_str()
                .ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'text' for type action".to_string(),
                    )
                })?
                .to_string();
            Ok(DesktopAction::Type { text })
        }
        "key_press" => {
            let keys = parse_string_array(args, "keys");
            Ok(DesktopAction::KeyPress { keys })
        }
        "scroll" => {
            let target = parse_click_target(args)?;
            let direction = match args["direction"].as_str() {
                Some("up") => ScrollDirection::Up,
                Some("down") => ScrollDirection::Down,
                Some("left") => ScrollDirection::Left,
                Some("right") => ScrollDirection::Right,
                _ => ScrollDirection::Down,
            };
            let amount = args["amount"].as_i64().unwrap_or(3) as i32;
            Ok(DesktopAction::Scroll { target, direction, amount })
        }
        "drag" => {
            let from_coords = (
                args["from_x"].as_i64().unwrap_or(0) as i32,
                args["from_y"].as_i64().unwrap_or(0) as i32,
            );
            let to_coords = (
                args["to_x"].as_i64().unwrap_or(0) as i32,
                args["to_y"].as_i64().unwrap_or(0) as i32,
            );
            let from = ClickTarget::Coordinate(Point::new(from_coords.0, from_coords.1));
            let to = ClickTarget::Coordinate(Point::new(to_coords.0, to_coords.1));
            Ok(DesktopAction::Drag { from, to })
        }
        "read_ui_tree" => {
            let app = args["app_name"].as_str().map(|s| s.to_string());
            Ok(DesktopAction::ReadUiTree { app })
        }
        "launch_app" => {
            let name = args["app_name"]
                .as_str()
                .ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'app_name' for launch_app action".to_string(),
                    )
                })?
                .to_string();
            let app_args = parse_string_array(args, "app_args");
            let wait_for_ready = args["wait_for_ready"].as_bool().unwrap_or(true);
            Ok(DesktopAction::LaunchApp {
                name,
                args: app_args,
                wait_for_ready,
            })
        }
        "activate_window" => {
            let title_pattern = args["title_pattern"]
                .as_str()
                .ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'title_pattern' for activate_window action".to_string(),
                    )
                })?
                .to_string();
            Ok(DesktopAction::ActivateWindow { title_pattern })
        }
        "close_window" => {
            let title_pattern = args["title_pattern"]
                .as_str()
                .ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'title_pattern' for close_window action".to_string(),
                    )
                })?
                .to_string();
            Ok(DesktopAction::CloseWindow { title_pattern })
        }
        "wait" => {
            let milliseconds = args["milliseconds"].as_u64().unwrap_or(1000);
            Ok(DesktopAction::Wait { milliseconds })
        }
        "clipboard_get" => Ok(DesktopAction::ClipboardGet),
        "clipboard_set" => {
            let text = args["text"]
                .as_str()
                .ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "Missing 'text' for clipboard_set action".to_string(),
                    )
                })?
                .to_string();
            Ok(DesktopAction::ClipboardSet { text })
        }
        "get_system_status" => Ok(DesktopAction::GetSystemStatus),
        "list_processes" => {
            let filter = args["filter"].as_str().map(|s| s.to_string());
            let limit = args["max_results"].as_u64().map(|n| n as usize);
            Ok(DesktopAction::ListProcesses { filter, limit })
        }
        "kill_process" => {
            let pid = args["pid"].as_u64().map(|n| n as u32);
            let name = args["name"].as_str().map(|s| s.to_string());
            let force = args["force"].as_bool().unwrap_or(false);
            Ok(DesktopAction::KillProcess { pid, name, force })
        }
        "restart_process" => {
            let pid = args["pid"].as_u64().map(|n| n as u32);
            let name = args["name"].as_str().map(|s| s.to_string());
            let force = args["force"].as_bool().unwrap_or(false);
            Ok(DesktopAction::RestartProcess { pid, name, force })
        }
        "set_process_priority" => {
            let pid = args["pid"].as_u64().map(|n| n as u32);
            let name = args["name"].as_str().map(|s| s.to_string());
            let priority = args["priority"].as_i64().unwrap_or(0) as i32;
            Ok(DesktopAction::SetProcessPriority { pid, name, priority })
        }
        "list_windows" => Ok(DesktopAction::ListWindows),
        "get_window_geometry" => {
            let title_pattern = parse_title_pattern(args, "get_window_geometry")?;
            Ok(DesktopAction::GetWindowGeometry { title_pattern })
        }
        "move_window" => {
            let title_pattern = parse_title_pattern(args, "move_window")?;
            let x = args["x"].as_i64().ok_or_else(|| {
                crate::error::SyscityError::Validation(
                    "Missing 'x' for move_window action".to_string(),
                )
            })? as i32;
            let y = args["y"].as_i64().ok_or_else(|| {
                crate::error::SyscityError::Validation(
                    "Missing 'y' for move_window action".to_string(),
                )
            })? as i32;
            Ok(DesktopAction::MoveWindow { title_pattern, x, y })
        }
        "resize_window" => {
            let title_pattern = parse_title_pattern(args, "resize_window")?;
            let width = args["width"].as_u64().ok_or_else(|| {
                crate::error::SyscityError::Validation(
                    "Missing 'width' for resize_window action".to_string(),
                )
            })? as u32;
            let height = args["height"].as_u64().ok_or_else(|| {
                crate::error::SyscityError::Validation(
                    "Missing 'height' for resize_window action".to_string(),
                )
            })? as u32;
            Ok(DesktopAction::ResizeWindow { title_pattern, width, height })
        }
        "minimize_window" => {
            let title_pattern = parse_title_pattern(args, "minimize_window")?;
            Ok(DesktopAction::MinimizeWindow { title_pattern })
        }
        "maximize_window" => {
            let title_pattern = parse_title_pattern(args, "maximize_window")?;
            Ok(DesktopAction::MaximizeWindow { title_pattern })
        }
        _ => Err(crate::error::SyscityError::Validation(format!(
            "Unknown computer action: {}",
            action
        ))),
    }
}

/// Parse an optional screenshot region from args.
fn parse_region(args: &Value) -> Option<Rect> {
    let x = args["region_x"].as_i64()?;
    let y = args["region_y"].as_i64()?;
    let w = args["region_width"].as_u64()?;
    let h = args["region_height"].as_u64()?;
    Some(Rect {
        x: x as i32,
        y: y as i32,
        width: w as u32,
        height: h as u32,
    })
}

/// Parse the required `title_pattern` arg for window actions.
fn parse_title_pattern(args: &Value, action: &str) -> crate::Result<String> {
    args["title_pattern"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| {
            crate::error::SyscityError::Validation(format!(
                "Missing 'title_pattern' for {} action",
                action
            ))
        })
}

/// Parse a click target from coordinates (x, y) or element parameters.
fn parse_click_target(args: &Value) -> crate::Result<ClickTarget> {
    // If x/y are provided, use coordinate target.
    if args.get("x").and_then(Value::as_i64).is_some()
        && args.get("y").and_then(Value::as_i64).is_some()
    {
        return Ok(ClickTarget::Coordinate(Point::new(
            args["x"].as_i64().unwrap_or(0) as i32,
            args["y"].as_i64().unwrap_or(0) as i32,
        )));
    }
    // Otherwise use element label if available.
    if let Some(label) = args["text"].as_str() {
        return Ok(ClickTarget::ElementLabel(label.to_string()));
    }
    // Fall back to coordinate (0, 0) — let the adapter handle it.
    Ok(ClickTarget::Coordinate(Point::new(0, 0)))
}

/// Parse mouse button from args.
fn parse_button(args: &Value) -> MouseButton {
    match args["button"].as_str() {
        Some("middle") => MouseButton::Middle,
        Some("right") => MouseButton::Right,
        _ => MouseButton::Left,
    }
}

/// Parse a string array from args key.
fn parse_string_array(args: &Value, key: &str) -> Vec<String> {
    args[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Convert an [`ActionResult`] to a [`ToolExecutionResult`].
///
/// Screenshots go through the content-addressed attachment store: the result
/// carries a compact reference plus a marker line, not megabytes of base64.
/// Fail-open: a store failure keeps the old inline-base64 behavior.
async fn action_result_to_tool_result(result: ActionResult) -> ToolExecutionResult {
    let mut output = result.message.clone();
    let mut data = result.data.clone();

    if let Some(screenshot) = result.screenshot_after {
        let stored = store_screenshot(&screenshot).await;
        match stored {
            Ok(Some(aref)) => {
                output.push_str(&format!(
                    "\n[Screenshot: {}x{} — attachment {} ({} bytes)]\n{}",
                    screenshot.width,
                    screenshot.height,
                    crate::attachments::short_id(&aref.digest),
                    aref.size,
                    crate::attachments::render_ref_line(&aref),
                ));
                let mut data_obj = data.unwrap_or_else(|| json!({}));
                if let Some(obj) = data_obj.as_object_mut() {
                    obj.insert("screenshot_ref".to_string(), aref.to_json());
                    obj.insert("screenshot_width".to_string(), json!(screenshot.width));
                    obj.insert("screenshot_height".to_string(), json!(screenshot.height));
                }
                data = Some(data_obj);
            }
            Ok(None) => {} // No payload at all (no base64, no file) — nothing to attach.
            Err(e) => {
                tracing::warn!(
                    "attachment store write failed ({}); falling back to inline base64",
                    e
                );
                let screenshot_info = format!(
                    "\n[Screenshot: {}x{} (base64 length: {})]",
                    screenshot.width,
                    screenshot.height,
                    screenshot.base64.len()
                );
                output.push_str(&screenshot_info);

                let mut data_obj = data.unwrap_or_else(|| json!({}));
                if let Some(obj) = data_obj.as_object_mut() {
                    obj.insert("screenshot_base64".to_string(), json!(screenshot.base64));
                    obj.insert("screenshot_width".to_string(), json!(screenshot.width));
                    obj.insert("screenshot_height".to_string(), json!(screenshot.height));
                }
                data = Some(data_obj);
            }
        }
    }

    if result.success {
        ToolExecutionResult::success(output).with_data(data.unwrap_or(json!({})))
    } else {
        ToolExecutionResult::error(output)
    }
}

/// Store a captured screenshot's payload in the attachment CAS.
///
/// Platform adapters deliver either in-memory base64 or (rarely) a file
/// path; `Ok(None)` means the screenshot carried no payload at all.
async fn store_screenshot(
    screenshot: &crate::computer::Screenshot,
) -> std::io::Result<Option<crate::attachments::AttachmentRef>> {
    use base64::Engine;
    let bytes: Vec<u8> = if !screenshot.base64.is_empty() {
        base64::engine::general_purpose::STANDARD
            .decode(&screenshot.base64)
            .map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("invalid base64: {e}"))
            })?
    } else if let Some(ref path) = screenshot.file_path {
        tokio::fs::read(path).await?
    } else {
        return Ok(None);
    };
    crate::attachments::store_bytes_async(bytes, "image/png")
        .await
        .map(Some)
}

/// Map [`ComputerError`] to [`SyscityError`].
fn to_syscity_err(e: ComputerError) -> crate::error::SyscityError {
    match e {
        ComputerError::UnsupportedPlatform(msg) => crate::error::SyscityError::Unsupported(msg),
        ComputerError::NoDisplay => {
            crate::error::SyscityError::Unsupported("No display server available".to_string())
        }
        ComputerError::AccessibilityDenied => crate::error::SyscityError::Unsupported(
            "Accessibility permission not granted".to_string(),
        ),
        other => crate::error::SyscityError::Internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{Result as ComputerResult, Screenshot, UiElement, WaitCondition};

    /// Mock adapter returning a fixed screenshot and empty UI tree.
    struct MockAdapter {
        screenshot_b64: String,
    }

    #[async_trait::async_trait]
    impl ComputerAdapter for MockAdapter {
        async fn screenshot(&self, _region: Option<Rect>) -> ComputerResult<Screenshot> {
            Ok(Screenshot::new(self.screenshot_b64.clone(), 100, 100))
        }

        async fn read_ui_tree(&self, _app: Option<&str>) -> ComputerResult<Vec<UiElement>> {
            Ok(Vec::new())
        }

        async fn execute(&self, _action: DesktopAction) -> ComputerResult<ActionResult> {
            Ok(ActionResult::success("done"))
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: std::time::Duration,
        ) -> ComputerResult<bool> {
            Ok(true)
        }
    }

    fn mock_tool() -> ComputerTool {
        ComputerTool::new(Some(Arc::new(MockAdapter {
            screenshot_b64: "aGVsbG8gd29ybGQ=".to_string(),
        })))
    }

    #[tokio::test]
    async fn mutating_action_appends_verification_summary() {
        let tool = mock_tool();
        let result = tool
            .execute(json!({"action": "click", "x": 10, "y": 20}), &ToolContext::default())
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result.output.contains("[verification]"),
            "click should append a verification summary, got: {}",
            result.output
        );
        // Structured diff attached to data.
        let data = result.data.expect("data should be present");
        assert!(data.get("verification").is_some());
    }

    #[tokio::test]
    async fn read_only_action_skips_verification() {
        let tool = mock_tool();
        let result = tool
            .execute(json!({"action": "clipboard_get"}), &ToolContext::default())
            .await
            .unwrap();
        assert!(!result.output.contains("[verification]"));
    }
}
