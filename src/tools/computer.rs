//! Tool wrapper around [`ComputerAdapter`] exposing desktop automation
//! operations as standard Tool trait implementations.
//!
//! The LLM calls `computer` with an `action` parameter to perform screenshots,
//! clicks, typing, key presses, window management, and other desktop ops.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

use crate::computer::session::{render_notes, ComputerSession, StepOutcome};
use crate::computer::{
    ActionResult, ClickTarget, ComputerAdapter, ComputerError, CoordinateSpace, DesktopAction,
    MouseButton, Point, Rect, ScrollDirection,
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
    /// The page side of the action ladder, when a browser is available.
    page: Option<Arc<dyn crate::computer::PageActor>>,
    /// One session per conversation: step counters, pinned notes and the
    /// rollback bookkeeping belong to the task, not to the process.
    sessions: tokio::sync::Mutex<std::collections::HashMap<String, Arc<ComputerSession>>>,
}

impl ComputerTool {
    pub fn new(adapter: Option<Arc<dyn ComputerAdapter>>) -> Self {
        Self {
            adapter,
            page: None,
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Attach the browser, so a click the screen cannot deliver can still be
    /// asked of the page. The page is scoped to the calling conversation.
    pub fn with_page_actor(mut self, actor: Arc<dyn crate::computer::PageActor>) -> Self {
        self.page = Some(actor);
        self
    }

    /// The session for this conversation, created on first use.
    ///
    /// Delivery is part of the session, not of the call: a session that was
    /// told to stay in the background must not inherit a foreground step's
    /// counted history by accident, and the reverse. Background sessions
    /// therefore get their own entry, keyed by mode.
    async fn session_for(
        &self,
        context: &ToolContext,
        delivery: crate::computer::DeliveryMode,
    ) -> Option<Arc<ComputerSession>> {
        let adapter = self.adapter.clone()?;
        let conversation = if context.conversation_id.is_empty() {
            "default".to_string()
        } else {
            context.conversation_id.clone()
        };
        let key = match delivery {
            crate::computer::DeliveryMode::Foreground => conversation.clone(),
            crate::computer::DeliveryMode::Background => format!("{conversation}#background"),
        };
        let mut sessions = self.sessions.lock().await;
        // Bound the map: a long-lived gateway sees many conversations, and a
        // session is only worth keeping while its task is live.
        if sessions.len() > 64 {
            sessions.clear();
        }
        let page = self.page.clone();
        Some(
            sessions
                .entry(key.clone())
                .or_insert_with(|| {
                    let session = ComputerSession::new(adapter).with_config(
                        crate::computer::session::SessionConfig { delivery, ..Default::default() },
                    );
                    match page {
                        // The page belongs to the conversation, not to the
                        // session key: the browser tool files tabs under the
                        // conversation id, and a `#background` suffix would
                        // look for a page nobody opened.
                        Some(actor) => Arc::new(session.with_page(actor, conversation)),
                        None => Arc::new(session),
                    }
                })
                .clone(),
        )
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
- "read_ui_tree" — inspect the accessibility tree of the active window

Coordinates: x/y are screen points by default. When you read a position off a screenshot, set `from_screenshot: true` so x/y are taken as pixels in that image — on a Retina display the image is twice the point size, and a pixel quoted as a point lands in the wrong place. `screen_state` reports the exact mapping in `coordinate_space`. An `element_id` needs no conversion."#
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
                        "resize_window", "minimize_window", "maximize_window",
                        "note", "recall_notes", "undo"
                    ],
                    "description": "The desktop operation to perform"
                },
                "element_id": {
                    "type": "string",
                    "description": "Element id from read_ui_tree / screen output, e.g. \"s0000001f:0.1\". Preferred over x/y for click and double_click: it resolves inside the application instead of at screen pixels, and survives the window moving. For type it names the field to write into, which lets the tool set its value directly instead of typing into whatever holds focus. An id from an older snapshot is rejected — re-read the tree."
                },
                "delivery": {
                    "type": "string",
                    "enum": ["foreground", "background"],
                    "description": "Where the input may land. \"foreground\" (default) may move the pointer and take focus. \"background\" acts on the app without bringing it forward: an element press or a field value goes through accessibility, and an action that can only be delivered by taking the screen (a bare key press, a click at a point, typing into whatever holds focus) is refused with a reason instead of delivered. Use it when the person at the machine keeps working, and pass an element_id so the action has something to aim at."
                },
                "note_key": {
                    "type": "string",
                    "description": "Key for action=note: the name this value is stored under (e.g. \"invoice_total\")."
                },
                // ── Common positional parameters ──────────────────────────
                "x": { "type": "integer", "description": "X coordinate (for click, double_click, scroll, drag_from, drag_to; for type it is the point to click into first)" },
                "y": { "type": "integer", "description": "Y coordinate" },
                "from_screenshot": {
                    "type": "boolean",
                    "description": "Interpret x/y (and from_x/from_y/to_x/to_y) as pixel coordinates in the most recent screenshot instead of screen points. Use this whenever the position was read off an image. The conversion comes from screen_state's coordinate_space; if none is known the tool takes one screenshot to establish it."
                },
                "text": { "type": "string", "description": "Text to type, or clipboard content to set. When typing, pair it with element_id (preferred) or x/y to name the field; without a target the text goes to whatever holds focus." },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Key names to press (e.g. [\"ctrl\",\"c\"], [\"cmd\",\"space\"], [\"enter\"], [\"tab\"], [\"escape\"])"
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
        mut args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let _t_start = std::time::Instant::now();
        // Owned: converting screenshot coordinates needs `&mut args` later.
        let action = args["action"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| crate::error::SyscityError::Validation("Missing action".to_string()))?;

        let delivery = match args.get("delivery").and_then(Value::as_str) {
            Some("background") => crate::computer::DeliveryMode::Background,
            _ => crate::computer::DeliveryMode::Foreground,
        };

        let Some(session) = self.session_for(_context, delivery).await else {
            return Err(crate::error::SyscityError::Unsupported(
                "Computer adapter is not configured".to_string(),
            ));
        };

        // Session actions are the agent's own memory, not desktop operations.
        match action.as_str() {
            "note" => {
                let key = args["note_key"]
                    .as_str()
                    .or_else(|| args["name"].as_str())
                    .ok_or_else(|| {
                        crate::error::SyscityError::Validation(
                            "Missing 'name' (the key) for note".to_string(),
                        )
                    })?;
                let value = args["text"].as_str().unwrap_or("");
                session.note(key, value).await;
                let notes = session.notes().await;
                return Ok(ToolExecutionResult::success(format!(
                    "Noted.\n{}",
                    render_notes(&notes)
                ))
                .with_data(json!({ "notes": serde_json::to_value(&notes).unwrap_or_default() })));
            }
            "recall_notes" => {
                let notes = session.notes().await;
                let rendered = if notes.is_empty() {
                    "No pinned notes.".to_string()
                } else {
                    render_notes(&notes)
                };
                return Ok(ToolExecutionResult::success(rendered).with_data(
                    json!({ "notes": serde_json::to_value(&notes).unwrap_or_default() }),
                ));
            }
            "undo" => {
                let undone = session.undo_last_step().await.map_err(to_syscity_err)?;
                let message = if undone {
                    "Rolled back the last snapshotted step."
                } else {
                    "Nothing to undo: no step in this session took a snapshot."
                };
                return Ok(
                    ToolExecutionResult::success(message).with_data(json!({ "undone": undone }))
                );
            }
            _ => {}
        }

        // A coordinate read off a screenshot is in the image's pixel space,
        // which on a Retina display is twice the point space clicks use.
        // Convert before parsing the action, so every coordinate parameter
        // goes through the same mapping.
        let mut coordinate_space = None;
        if args["from_screenshot"].as_bool().unwrap_or(false)
            && args["element_id"].as_str().is_none_or(str::is_empty)
            && has_screenshot_coordinates(&args)
        {
            let space = match session.coordinate_space().await {
                Some(space) => space,
                None => session
                    .refresh_coordinate_space()
                    .await
                    .map_err(to_syscity_err)?,
            };
            let converted = convert_screenshot_coordinates(&mut args, &space);
            info!(
                "[ComputerTool] converted {} coordinate pair(s) via {}",
                converted,
                space.summary()
            );
            coordinate_space = Some(space);
        }

        let desktop_action = action_to_desktop_action(&action, &args)?;

        let adapter_start = std::time::Instant::now();
        let outcome = session
            .execute(desktop_action)
            .await
            .map_err(to_syscity_err)?;
        info!(
            "[ComputerTool] action={} session.execute() took {:?} (total so far: {:?})",
            action,
            adapter_start.elapsed(),
            _t_start.elapsed()
        );

        let mut tool_result = build_tool_result(outcome, &action).await;

        // Say which space the coordinates were quoted in, so the caller can
        // see the conversion rather than having to trust it.
        if let Some(space) = coordinate_space {
            let data = tool_result.data.get_or_insert_with(|| json!({}));
            if let Some(obj) = data.as_object_mut() {
                obj.insert("from_screenshot".to_string(), json!(true));
                obj.insert(
                    "coordinate_space".to_string(),
                    serde_json::to_value(space).unwrap_or_default(),
                );
            }
        }

        // Re-perception is when memory earns its keep: a model that reads the
        // screen again after 200 steps gets its earlier findings back.
        if matches!(action.as_str(), "screenshot" | "read_ui_tree") {
            let notes = session.notes().await;
            if !notes.is_empty() {
                tool_result
                    .output
                    .push_str(&format!("\n\n{}", render_notes(&notes)));
                let data = tool_result.data.get_or_insert_with(|| json!({}));
                if let Some(obj) = data.as_object_mut() {
                    obj.insert(
                        "notes".to_string(),
                        serde_json::to_value(&notes).unwrap_or_default(),
                    );
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
            Ok(DesktopAction::Type {
                text,
                target: parse_type_target(args),
            })
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

/// Coordinate parameter pairs an action may carry.
const COORDINATE_PAIRS: [(&str, &str); 3] = [("x", "y"), ("from_x", "from_y"), ("to_x", "to_y")];

/// Whether the args carry at least one complete coordinate pair.
fn has_screenshot_coordinates(args: &Value) -> bool {
    COORDINATE_PAIRS.iter().any(|(xk, yk)| {
        args.get(*xk).and_then(Value::as_i64).is_some()
            && args.get(*yk).and_then(Value::as_i64).is_some()
    })
}

/// Convert every coordinate pair in `args` from screenshot pixels to screen
/// points. Returns the number of pairs converted.
fn convert_screenshot_coordinates(args: &mut Value, space: &CoordinateSpace) -> usize {
    let mut converted = 0;
    for (xk, yk) in COORDINATE_PAIRS {
        let x = args.get(xk).and_then(Value::as_i64);
        let y = args.get(yk).and_then(Value::as_i64);
        if let (Some(x), Some(y)) = (x, y) {
            let (sx, sy) = space.image_to_screen(x as i32, y as i32);
            args[xk] = json!(sx);
            args[yk] = json!(sy);
            converted += 1;
        }
    }
    converted
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
    // An element id is the most precise target: it names an element in a
    // tree the caller was shown, not a pixel on a screen that may move.
    if let Some(id) = args["element_id"].as_str().filter(|s| !s.is_empty()) {
        return Ok(ClickTarget::ElementId(id.to_string()));
    }
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

/// Parse the field a `type` action should write into, when one was named.
///
/// Unlike `parse_click_target`, `text` is never read as a label here: for a
/// type action `text` is the content, so treating it as a target would aim
/// the action at a field named after the words it is about to type.
fn parse_type_target(args: &Value) -> Option<ClickTarget> {
    if let Some(id) = args["element_id"].as_str().filter(|s| !s.is_empty()) {
        return Some(ClickTarget::ElementId(id.to_string()));
    }
    if let Some(label) = args["element_label"].as_str().filter(|s| !s.is_empty()) {
        return Some(ClickTarget::ElementLabel(label.to_string()));
    }
    match (args.get("x").and_then(Value::as_i64), args.get("y").and_then(Value::as_i64)) {
        (Some(x), Some(y)) => Some(ClickTarget::Coordinate(Point::new(x as i32, y as i32))),
        _ => None,
    }
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

/// Render a [`StepOutcome`] for the model.
///
/// The payload carries machine-readable `effect` / `escalation` / `rung`, and
/// the text carries the same facts in one line each, because the model reads
/// the text and the harness reads the data.
async fn build_tool_result(outcome: StepOutcome, action: &str) -> ToolExecutionResult {
    let StepOutcome {
        result,
        diff,
        evidence,
        sensitive_after,
        sensitive_target,
        reflection,
        verification,
        ..
    } = outcome;

    let mut tool_result = action_result_to_tool_result(result).await;

    if let Some(diff) = &diff {
        tool_result
            .output
            .push_str(&format!("\n\n[verification] {}", diff.summary()));
    }

    if let Some(evidence) = &evidence {
        tool_result
            .output
            .push_str(&format!("\n[action] {}", evidence.summary()));
    }

    if let Some(analysis) = &reflection {
        tool_result.output.push_str(&format!(
            "\n[failure] cause={:?} confidence={:.2}: {}",
            analysis.root_cause, analysis.confidence, analysis.details
        ));
        if let Some(fix) = &analysis.suggested_fix {
            tool_result
                .output
                .push_str(&format!("\n[suggested] {}", fix));
        }
    }

    if sensitive_target || !sensitive_after.is_empty() {
        let categories: Vec<String> = sensitive_after
            .iter()
            .map(|f| f.category.as_str().to_string())
            .collect();
        let mut line = String::from("\n[sensitive-ui]");
        if sensitive_target {
            line.push_str(" this action touched a sensitive control.");
        }
        if !categories.is_empty() {
            line.push_str(&format!(" detected: {}", categories.join(", ")));
        }
        line.push_str(" Do not enter credentials or confirm payments/destructive prompts unless the user asked for it.");
        tool_result.output.push_str(&line);
    }

    let data = tool_result.data.get_or_insert_with(|| json!({}));
    if let Some(obj) = data.as_object_mut() {
        if let Some(diff) = &diff {
            obj.insert("verification".to_string(), serde_json::to_value(diff).unwrap_or_default());
        }
        if let Some(evidence) = &evidence {
            obj.insert("rung".to_string(), json!(rung_name(evidence.rung)));
            obj.insert("effect".to_string(), json!(evidence.effect.as_str()));
            if let Some(next) = evidence.escalation {
                obj.insert("escalation".to_string(), json!(escalation_name(next)));
            }
            if let Some(recovery) = &evidence.recovery {
                obj.insert("recovery".to_string(), json!(recovery));
            }
            obj.insert(
                "tried".to_string(),
                serde_json::to_value(&evidence.tried).unwrap_or_default(),
            );
        }
        obj.insert("verification_state".to_string(), json!(verification_name(verification)));
        if !sensitive_after.is_empty() || sensitive_target {
            obj.insert("sensitive_target".to_string(), json!(sensitive_target));
            obj.insert(
                "sensitive_ui".to_string(),
                serde_json::to_value(&sensitive_after).unwrap_or_default(),
            );
        }
        if let Some(analysis) = &reflection {
            obj.insert(
                "reflection".to_string(),
                serde_json::to_value(analysis).unwrap_or_default(),
            );
        }
        obj.insert("action".to_string(), json!(action));
    }

    // A failed result reaches the model through its `error` field only, so the
    // evidence and reflection appended to `output` would be invisible — and a
    // refusal without its recovery hint is just a dead end.
    if !tool_result.success {
        let extra = std::mem::take(&mut tool_result.output);
        if !extra.trim().is_empty() {
            let base = tool_result.error.take().unwrap_or_default();
            tool_result.error = Some(format!("{base}{extra}"));
        }
    }

    tool_result
}

fn rung_name(rung: crate::computer::Rung) -> &'static str {
    match rung {
        crate::computer::Rung::AxAction => "ax_action",
        crate::computer::Rung::Pixel => "pixel",
        crate::computer::Rung::Keyboard => "keyboard",
        crate::computer::Rung::Page => "page",
    }
}

fn escalation_name(escalation: crate::computer::Escalation) -> &'static str {
    match escalation {
        crate::computer::Escalation::Px => "px",
        crate::computer::Escalation::Foreground => "foreground",
        crate::computer::Escalation::Page => "page",
    }
}

fn verification_name(state: crate::computer::use_loop::StepVerification) -> &'static str {
    match state {
        crate::computer::use_loop::StepVerification::Verified => "verified",
        crate::computer::use_loop::StepVerification::Failed => "failed",
        crate::computer::use_loop::StepVerification::Unverifiable => "unverifiable",
    }
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

        async fn display_geometry(&self) -> ComputerResult<crate::computer::DisplayGeometry> {
            // 100x100 screenshot, 50x50 points: a 2x capture, like Retina.
            Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 50, 50,
            )
            .primary()]))
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
        // ... and so is a machine-readable verdict. The mock's screen never
        // changes, so the click is a suspected no-op with a next rung to try.
        assert_eq!(data["effect"], json!("suspected_noop"));
        assert_eq!(data["rung"], json!("pixel"));
        assert_eq!(data["escalation"], json!("foreground"));
        assert_eq!(data["verification_state"], json!("failed"));
    }

    #[test]
    fn screenshot_coordinates_convert_and_leave_other_args_alone() {
        let geometry =
            crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 50, 50,
            )
            .primary()]);
        let space = CoordinateSpace::new(100, 100, &geometry).unwrap();
        let mut args = json!({
            "x": 80, "y": 40,
            "from_x": 10, "from_y": 20,
            "to_x": 100, "to_y": 100,
            "text": "hello",
        });
        assert_eq!(convert_screenshot_coordinates(&mut args, &space), 3);
        assert_eq!((args["x"].as_i64(), args["y"].as_i64()), (Some(40), Some(20)));
        assert_eq!((args["from_x"].as_i64(), args["from_y"].as_i64()), (Some(5), Some(10)));
        assert_eq!((args["to_x"].as_i64(), args["to_y"].as_i64()), (Some(50), Some(50)));
        assert_eq!(args["text"], json!("hello"));
    }

    #[test]
    fn incomplete_coordinate_pairs_are_not_converted() {
        assert!(!has_screenshot_coordinates(&json!({"x": 1})));
        assert!(!has_screenshot_coordinates(&json!({"action": "click"})));
        assert!(has_screenshot_coordinates(&json!({"x": 1, "y": 2})));
    }

    #[tokio::test]
    async fn from_screenshot_converts_click_and_reports_the_space() {
        let tool = mock_tool();
        let result = tool
            .execute(
                json!({"action": "click", "x": 80, "y": 40, "from_screenshot": true}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        let data = result.data.expect("data should be present");
        assert_eq!(data["from_screenshot"], json!(true));
        assert_eq!(data["coordinate_space"]["image_width"], json!(100));
        assert_eq!(data["coordinate_space"]["screen_width"], json!(50));
    }

    #[test]
    fn a_type_action_aims_at_the_named_field_not_at_its_own_text() {
        let args = json!({"action": "type", "text": "hello", "element_id": "s00000001:0.2"});
        match action_to_desktop_action("type", &args).unwrap() {
            DesktopAction::Type { text, target } => {
                assert_eq!(text, "hello");
                assert_eq!(target, Some(ClickTarget::ElementId("s00000001:0.2".to_string())));
            }
            other => panic!("expected Type, got {other:?}"),
        }

        // Without an element the text must not be mistaken for a label: a
        // field named "hello" is not what the caller meant to type into.
        let args = json!({"action": "type", "text": "hello"});
        match action_to_desktop_action("type", &args).unwrap() {
            DesktopAction::Type { target, .. } => assert!(target.is_none()),
            other => panic!("expected Type, got {other:?}"),
        }

        let args = json!({"action": "type", "text": "hello", "x": 12, "y": 34});
        match action_to_desktop_action("type", &args).unwrap() {
            DesktopAction::Type { target, .. } => {
                assert_eq!(target, Some(ClickTarget::Coordinate(Point::new(12, 34))));
            }
            other => panic!("expected Type, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn element_id_clicks_need_no_conversion() {
        // No screenshot has passed through the session, so a conversion would
        // have to take one; an element-id click must not pay for that.
        let tool = mock_tool();
        let result = tool
            .execute(
                json!({"action": "click", "element_id": "s00000001:0.1", "x": 80, "y": 40, "from_screenshot": true}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        let data = result.data.expect("data should be present");
        assert!(data.get("coordinate_space").is_none(), "{data}");
    }

    /// Adapter that refuses element clicks as stale.
    struct StaleClickAdapter;

    #[async_trait::async_trait]
    impl ComputerAdapter for StaleClickAdapter {
        async fn screenshot(&self, _region: Option<Rect>) -> ComputerResult<Screenshot> {
            Ok(Screenshot::new(String::new(), 100, 100))
        }

        async fn read_ui_tree(&self, _app: Option<&str>) -> ComputerResult<Vec<UiElement>> {
            Ok(Vec::new())
        }

        async fn execute(&self, action: DesktopAction) -> ComputerResult<ActionResult> {
            match action {
                DesktopAction::Click { .. } | DesktopAction::DoubleClick { .. } => {
                    Err(ComputerError::StaleElement("s00000001:0.1".to_string()))
                }
                _ => Ok(ActionResult::success("ok")),
            }
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: std::time::Duration,
        ) -> ComputerResult<bool> {
            Ok(true)
        }

        async fn display_geometry(&self) -> ComputerResult<crate::computer::DisplayGeometry> {
            Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 100, 100,
            )
            .primary()]))
        }
    }

    #[tokio::test]
    async fn a_refusal_reaches_the_model_with_its_recovery_hint() {
        // A failed tool result is rendered from `error` alone; the evidence
        // appended to `output` must be merged in, or the refusal arrives as a
        // bare message with nothing the model can act on.
        let tool = ComputerTool::new(Some(Arc::new(StaleClickAdapter)));
        let result = tool
            .execute(
                json!({"action": "click", "element_id": "s00000001:0.1"}),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!result.success);
        let error = result.error.expect("error text");
        assert!(error.contains("refused"), "{error}");
        assert!(error.contains("re-read the UI tree"), "{error}");
        let data = result.data.expect("data");
        assert_eq!(data["effect"], json!("refused"));
        assert!(data["recovery"]
            .as_str()
            .unwrap_or_default()
            .contains("re-read the UI tree"));
    }

    #[tokio::test]
    async fn pinned_notes_come_back_on_reperception() {
        let tool = mock_tool();
        let ctx = ToolContext::default();
        tool.execute(json!({"action": "note", "note_key": "invoice", "text": "INV-42"}), &ctx)
            .await
            .unwrap();
        let result = tool
            .execute(json!({"action": "read_ui_tree"}), &ctx)
            .await
            .unwrap();
        assert!(result.output.contains("invoice = INV-42"), "{}", result.output);
        let data = result.data.expect("data should be present");
        assert_eq!(data["notes"][0]["key"], json!("invoice"));
    }

    #[tokio::test]
    async fn sessions_are_scoped_to_a_conversation() {
        let tool = mock_tool();
        let mut alice = ToolContext::default();
        alice.identity.conversation_id = "alice".to_string();
        let mut bob = ToolContext::default();
        bob.identity.conversation_id = "bob".to_string();

        tool.execute(json!({"action": "note", "note_key": "secret", "text": "one"}), &alice)
            .await
            .unwrap();

        let bob_notes = tool
            .execute(json!({"action": "recall_notes"}), &bob)
            .await
            .unwrap();
        assert!(!bob_notes.output.contains("secret"), "{}", bob_notes.output);
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
