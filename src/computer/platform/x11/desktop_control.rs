//! Linux X11 desktop control tool using `xdotool`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::info;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Action to perform on the X11 desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopAction {
    /// Get active window info.
    Inspect,
    /// Click at coordinates or on a window.
    Click {
        x: Option<i32>,
        y: Option<i32>,
        window_id: Option<String>,
        button: Option<u8>,
    },
    /// Type text.
    Type { text: String },
    /// Press keyboard keys.
    Key { keys: Vec<String> },
    /// Get window list.
    ListWindows,
    /// Activate a window by name or ID.
    ActivateWindow {
        name: Option<String>,
        window_id: Option<String>,
    },
}

/// Desktop control tool for Linux X11 via `xdotool`.
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

    async fn run_xdotool(args: &[&str]) -> crate::Result<(bool, String, String)> {
        let output =
            timeout(Duration::from_secs(10), Command::new("xdotool").args(args).output()).await;
        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok((out.status.success(), stdout, stderr))
            }
            Ok(Err(e)) => Ok((false, String::new(), format!("xdotool spawn error: {}", e))),
            Err(_) => Ok((false, String::new(), "xdotool timed out".to_string())),
        }
    }

    async fn run_wmctrl(args: &[&str]) -> crate::Result<(bool, String, String)> {
        let output =
            timeout(Duration::from_secs(10), Command::new("wmctrl").args(args).output()).await;
        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok((out.status.success(), stdout, stderr))
            }
            Ok(Err(e)) => Ok((false, String::new(), format!("wmctrl spawn error: {}", e))),
            Err(_) => Ok((false, String::new(), "wmctrl timed out".to_string())),
        }
    }
}

#[async_trait]
impl Tool for DesktopControlTool {
    fn name(&self) -> &str {
        "linux_x11_desktop_control"
    }

    fn description(&self) -> &str {
        "Control the Linux X11 desktop using xdotool. Supports click, type, key presses, window \
         inspection, and window activation. Input goes to the live session: it moves the real \
         pointer and types into whatever holds focus, so on a desktop someone is using, say what \
         you intend and ask before acting."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Perform a desktop control action",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action to perform: inspect, click, type, key, list_windows, activate_window",
                    "enum": ["inspect", "click", "double_click", "type", "key", "scroll", "drag", "list_windows", "activate_window", "close_window", "get_window_geometry", "move_window", "resize_window", "minimize_window", "maximize_window"]
                },
                "x": {
                    "type": "integer",
                    "description": "X coordinate for click"
                },
                "y": {
                    "type": "integer",
                    "description": "Y coordinate for click"
                },
                "window_id": {
                    "type": "string",
                    "description": "Window ID for click or activate"
                },
                "button": {
                    "type": "integer",
                    "description": "Mouse button (1=left, 2=middle, 3=right)",
                    "default": 1
                },
                "direction": {
                    "type": "string",
                    "description": "Scroll direction",
                    "enum": ["up", "down", "left", "right"]
                },
                "amount": {
                    "type": "integer",
                    "description": "Scroll amount (number of wheel clicks)",
                    "default": 3
                },
                "from_x": {
                    "type": "integer",
                    "description": "Start X coordinate for drag"
                },
                "from_y": {
                    "type": "integer",
                    "description": "Start Y coordinate for drag"
                },
                "to_x": {
                    "type": "integer",
                    "description": "End X coordinate for drag"
                },
                "to_y": {
                    "type": "integer",
                    "description": "End Y coordinate for drag"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type"
                },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keys to press (e.g. [\"ctrl\", \"c\"])"
                },
                "name": {
                    "type": "string",
                    "description": "Window name for activate"
                },
                "width": {
                    "type": "integer",
                    "description": "Target width (resize_window)"
                },
                "height": {
                    "type": "integer",
                    "description": "Target height (resize_window)"
                }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // One pointer and one focus: two conversations must not drive this
        // display at once. See `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::DESKTOP)
            .await;
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("inspect");

        info!("X11 desktop control action: {}", action);

        match action {
            "inspect" => {
                let (ok, active, err) = Self::run_xdotool(&["getactivewindow"]).await?;
                if !ok {
                    return Ok(ToolExecutionResult::error(format!(
                        "getactivewindow failed: {}",
                        err
                    )));
                }
                let win_id = active.trim();

                let (ok2, name, err2) = Self::run_xdotool(&["getwindowname", win_id]).await?;
                let window_name = if ok2 { name } else { err2 };

                let (ok3, pid, _) = Self::run_xdotool(&["getwindowpid", win_id]).await?;
                let window_pid = if ok3 { pid } else { "unknown".to_string() };

                let (ok4, geometry, _) = Self::run_xdotool(&["getwindowgeometry", win_id]).await?;
                let geo = if ok4 { geometry } else { "unknown".to_string() };

                let output = format!(
                    "Active window:\n  ID: {}\n  Name: {}\n  PID: {}\n  Geometry: {}",
                    win_id, window_name, window_pid, geo
                );

                Ok(ToolExecutionResult::success(output).with_data(serde_json::json!({
                    "window_id": win_id,
                    "window_name": window_name,
                    "pid": window_pid,
                    "geometry": geo
                })))
            }
            "click" => {
                let x = args.get("x").and_then(|v| v.as_i64());
                let y = args.get("y").and_then(|v| v.as_i64());
                let button = args.get("button").and_then(|v| v.as_u64()).unwrap_or(1);
                let window_id = args.get("window_id").and_then(|v| v.as_str());

                if let (Some(xv), Some(yv)) = (x, y) {
                    let _ =
                        Self::run_xdotool(&["mousemove", &format!("{}", xv), &format!("{}", yv)])
                            .await?;
                } else if let Some(wid) = window_id {
                    let _ = Self::run_xdotool(&["windowfocus", wid]).await?;
                    let _ = Self::run_xdotool(&["windowactivate", wid]).await?;
                }

                let (ok, _, err) = Self::run_xdotool(&["click", &format!("{}", button)]).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Clicked button {}", button)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Click failed: {}", err)))
                }
            }
            "type" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let (ok, _, err) = Self::run_xdotool(&["type", text]).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Typed text: {}", text)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Type failed: {}", err)))
                }
            }
            "key" => {
                let keys: Vec<String> = args
                    .get("keys")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let key_str = keys.join("+");
                let (ok, _, err) = Self::run_xdotool(&["key", &key_str]).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Pressed keys: {}", key_str)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Key press failed: {}", err)))
                }
            }
            "double_click" => {
                let x = args.get("x").and_then(|v| v.as_i64());
                let y = args.get("y").and_then(|v| v.as_i64());
                let button = args.get("button").and_then(|v| v.as_u64()).unwrap_or(1);

                if let (Some(xv), Some(yv)) = (x, y) {
                    let _ =
                        Self::run_xdotool(&["mousemove", &format!("{}", xv), &format!("{}", yv)])
                            .await?;
                }

                let (ok, _, err) =
                    Self::run_xdotool(&["click", "--repeat", "2", &format!("{}", button)]).await?;
                if ok {
                    Ok(ToolExecutionResult::success(format!("Double-clicked button {}", button)))
                } else {
                    Ok(ToolExecutionResult::error(format!("Double-click failed: {}", err)))
                }
            }
            "scroll" => {
                let x = args.get("x").and_then(|v| v.as_i64());
                let y = args.get("y").and_then(|v| v.as_i64());
                let direction = args
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("down");
                let amount = args.get("amount").and_then(|v| v.as_u64()).unwrap_or(3);

                if let (Some(xv), Some(yv)) = (x, y) {
                    let _ =
                        Self::run_xdotool(&["mousemove", &format!("{}", xv), &format!("{}", yv)])
                            .await?;
                }

                let btn = match direction {
                    "up" => "4",
                    "down" => "5",
                    "left" => "6",
                    "right" => "7",
                    _ => "5",
                };

                for _ in 0..amount {
                    let _ = Self::run_xdotool(&["click", btn]).await?;
                }

                Ok(ToolExecutionResult::success(format!(
                    "Scrolled {} ({} clicks)",
                    direction, amount
                )))
            }
            "drag" => {
                let from_x = args.get("from_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let from_y = args.get("from_y").and_then(|v| v.as_i64()).unwrap_or(0);
                let to_x = args.get("to_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let to_y = args.get("to_y").and_then(|v| v.as_i64()).unwrap_or(0);

                let _ = Self::run_xdotool(&[
                    "mousemove",
                    &format!("{}", from_x),
                    &format!("{}", from_y),
                ])
                .await?;
                let _ = Self::run_xdotool(&["mousedown", "1"]).await?;
                let _ =
                    Self::run_xdotool(&["mousemove", &format!("{}", to_x), &format!("{}", to_y)])
                        .await?;
                let (ok, _, err) = Self::run_xdotool(&["mouseup", "1"]).await?;

                if ok {
                    Ok(ToolExecutionResult::success(format!(
                        "Dragged from ({}, {}) to ({}, {})",
                        from_x, from_y, to_x, to_y
                    )))
                } else {
                    Ok(ToolExecutionResult::error(format!("Drag failed: {}", err)))
                }
            }
            "activate_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                let window_id = args.get("window_id").and_then(|v| v.as_str());

                if let Some(wid) = window_id {
                    let (ok, _, err) = Self::run_xdotool(&["windowactivate", wid]).await?;
                    if ok {
                        return Ok(ToolExecutionResult::success(format!(
                            "Activated window {}",
                            wid
                        )));
                    } else {
                        return Ok(ToolExecutionResult::error(format!("Activate failed: {}", err)));
                    }
                }

                if let Some(n) = name {
                    let (ok, wid, err) = Self::run_xdotool(&["search", "--name", n]).await?;
                    if ok && !wid.trim().is_empty() {
                        let first = wid.lines().next().unwrap_or("").trim();
                        let (ok2, _, err2) = Self::run_xdotool(&["windowactivate", first]).await?;
                        if ok2 {
                            return Ok(ToolExecutionResult::success(format!(
                                "Activated window '{}' (id: {})",
                                n, first
                            )));
                        } else {
                            return Ok(ToolExecutionResult::error(format!(
                                "Activate failed: {}",
                                err2
                            )));
                        }
                    }
                    return Ok(ToolExecutionResult::error(format!(
                        "Window '{}' not found: {}",
                        n, err
                    )));
                }

                Ok(ToolExecutionResult::error("Provide either 'name' or 'window_id'".to_string()))
            }
            "close_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                let window_id = args.get("window_id").and_then(|v| v.as_str());

                if let Some(wid) = window_id {
                    let (ok, _, err) = Self::run_xdotool(&["windowclose", wid]).await?;
                    if ok {
                        return Ok(ToolExecutionResult::success(format!("Closed window {}", wid)));
                    } else {
                        return Ok(ToolExecutionResult::error(format!("Close failed: {}", err)));
                    }
                }

                if let Some(n) = name {
                    let (ok, wid, err) = Self::run_xdotool(&["search", "--name", n]).await?;
                    if ok && !wid.trim().is_empty() {
                        let first = wid.lines().next().unwrap_or("").trim();
                        let (ok2, _, err2) = Self::run_xdotool(&["windowclose", first]).await?;
                        if ok2 {
                            return Ok(ToolExecutionResult::success(format!(
                                "Closed window '{}' (id: {})",
                                n, first
                            )));
                        } else {
                            return Ok(ToolExecutionResult::error(format!(
                                "Close failed: {}",
                                err2
                            )));
                        }
                    }
                    return Ok(ToolExecutionResult::error(format!(
                        "Window '{}' not found: {}",
                        n, err
                    )));
                }

                Ok(ToolExecutionResult::error(
                    "Provide 'name' or 'window_id' for close_window".to_string(),
                ))
            }
            // ── Window management ──────────────────────────────────────────
            "list_windows" => {
                // Enhanced with wmctrl for structured output
                let mut output = String::new();
                if let Ok((ok, list, _)) = Self::run_wmctrl(&["-l", "-p"]).await {
                    if ok && !list.trim().is_empty() {
                        output.push_str(&list);
                    }
                }
                if output.is_empty() {
                    let (ok, stdout, err) =
                        Self::run_xdotool(&["search", "--onlyvisible", ".*"]).await?;
                    if ok {
                        output = stdout;
                    } else {
                        return Ok(ToolExecutionResult::error(format!(
                            "List windows failed: {}",
                            err
                        )));
                    }
                }
                Ok(ToolExecutionResult::success(format!("Windows:\n{}", output)))
            }
            "get_window_geometry" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let (ok, wid, err) = Self::run_xdotool(&["search", "--name", n]).await?;
                    if ok {
                        let first = wid.lines().next().unwrap_or("").trim();
                        if !first.is_empty() {
                            let (ok2, geo, err2) =
                                Self::run_xdotool(&["getwindowgeometry", first]).await?;
                            if ok2 {
                                return Ok(ToolExecutionResult::success(geo));
                            } else {
                                return Ok(ToolExecutionResult::error(format!(
                                    "Get geometry failed: {}",
                                    err2
                                )));
                            }
                        }
                    }
                    return Ok(ToolExecutionResult::error(format!(
                        "Window '{}' not found: {}",
                        n, err
                    )));
                }
                Ok(ToolExecutionResult::error("Provide 'name' for get_window_geometry".to_string()))
            }
            "move_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                let x = args.get("x").and_then(|v| v.as_i64());
                let y = args.get("y").and_then(|v| v.as_i64());
                if let (Some(n), Some(xv), Some(yv)) = (name, x, y) {
                    let (ok, wid, err) = Self::run_xdotool(&["search", "--name", n]).await?;
                    if ok {
                        let first = wid.lines().next().unwrap_or("").trim();
                        if !first.is_empty() {
                            let (ok2, _, err2) = Self::run_xdotool(&[
                                "windowmove",
                                first,
                                &xv.to_string(),
                                &yv.to_string(),
                            ])
                            .await?;
                            if ok2 {
                                return Ok(ToolExecutionResult::success(format!(
                                    "Moved window '{}' to ({}, {})",
                                    n, xv, yv
                                )));
                            } else {
                                return Ok(ToolExecutionResult::error(format!(
                                    "Move failed: {}",
                                    err2
                                )));
                            }
                        }
                    }
                    return Ok(ToolExecutionResult::error(format!(
                        "Window '{}' not found: {}",
                        n, err
                    )));
                }
                Ok(ToolExecutionResult::error(
                    "Provide 'name', 'x', 'y' for move_window".to_string(),
                ))
            }
            "resize_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                let width = args.get("width").and_then(|v| v.as_u64());
                let height = args.get("height").and_then(|v| v.as_u64());
                if let (Some(n), Some(w), Some(h)) = (name, width, height) {
                    let (ok, wid, err) = Self::run_xdotool(&["search", "--name", n]).await?;
                    if ok {
                        let first = wid.lines().next().unwrap_or("").trim();
                        if !first.is_empty() {
                            let (ok2, _, err2) = Self::run_xdotool(&[
                                "windowsize",
                                first,
                                &w.to_string(),
                                &h.to_string(),
                            ])
                            .await?;
                            if ok2 {
                                return Ok(ToolExecutionResult::success(format!(
                                    "Resized window '{}' to {}x{}",
                                    n, w, h
                                )));
                            } else {
                                return Ok(ToolExecutionResult::error(format!(
                                    "Resize failed: {}",
                                    err2
                                )));
                            }
                        }
                    }
                    return Ok(ToolExecutionResult::error(format!(
                        "Window '{}' not found: {}",
                        n, err
                    )));
                }
                Ok(ToolExecutionResult::error(
                    "Provide 'name', 'width', 'height' for resize_window".to_string(),
                ))
            }
            "minimize_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let (ok, wid, err) = Self::run_xdotool(&["search", "--name", n]).await?;
                    if ok {
                        let first = wid.lines().next().unwrap_or("").trim();
                        if !first.is_empty() {
                            let (ok2, _, err2) =
                                Self::run_xdotool(&["windowminimize", first]).await?;
                            if ok2 {
                                return Ok(ToolExecutionResult::success(format!(
                                    "Minimized window '{}'",
                                    n
                                )));
                            } else {
                                return Ok(ToolExecutionResult::error(format!(
                                    "Minimize failed: {}",
                                    err2
                                )));
                            }
                        }
                    }
                    return Ok(ToolExecutionResult::error(format!(
                        "Window '{}' not found: {}",
                        n, err
                    )));
                }
                Ok(ToolExecutionResult::error("Provide 'name' for minimize_window".to_string()))
            }
            "maximize_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let (ok, stdout, err) =
                        Self::run_wmctrl(&["-r", n, "-b", "toggle,maximized_vert,maximized_horz"])
                            .await?;
                    if ok {
                        Ok(ToolExecutionResult::success(format!(
                            "Maximize toggled for '{}': {}",
                            n, stdout
                        )))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Maximize failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error("Provide 'name' for maximize_window".to_string()))
                }
            }
            _ => Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        std::env::var("DISPLAY").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_control_tool_creation() {
        let tool = DesktopControlTool::new();
        assert_eq!(tool.name(), "linux_x11_desktop_control");
    }
}
