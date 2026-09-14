//! Linux Wayland desktop control tool using `ydotool` or `wtype`.

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::info;

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Desktop control tool for Linux Wayland via `ydotool` / `wtype`.
///
/// Wayland's security model restricts arbitrary window manipulation,
/// so this focuses on input simulation (click, type, key) rather than
/// window introspection.
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

    /// Find the first available input simulation tool.
    async fn find_input_tool() -> Option<&'static str> {
        for cmd in &["ydotool", "wtype"] {
            if Command::new("which")
                .arg(cmd)
                .output()
                .await
                .ok()
                .is_some_and(|o| o.status.success())
            {
                return Some(cmd);
            }
        }
        None
    }

    async fn run_cmd(cmd: &str, args: &[&str]) -> crate::Result<(bool, String, String)> {
        let output = timeout(Duration::from_secs(10), Command::new(cmd).args(args).output()).await;
        match output {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Ok((out.status.success(), stdout, stderr))
            }
            Ok(Err(e)) => Ok((false, String::new(), format!("{} spawn error: {}", cmd, e))),
            Err(_) => Ok((false, String::new(), format!("{} timed out", cmd))),
        }
    }
}

#[async_trait]
impl Tool for DesktopControlTool {
    fn name(&self) -> &str {
        "linux_wayland_desktop_control"
    }

    fn description(&self) -> &str {
        "Control the Linux Wayland desktop using ydotool or wtype. Supports mouse click, type \
         text, and key presses. Note: Wayland restricts window introspection; window management is \
         limited compared to X11. Input goes to the live session: it moves the real pointer and \
         types into whatever holds focus, so on a desktop someone is using, say what you intend \
         and ask before acting."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Perform a desktop control action",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "Action: click, type, key",
                    "enum": ["click", "double_click", "type", "key", "scroll", "drag", "close_window", "activate_window"]
                },
                "x": {
                    "type": "integer",
                    "description": "X coordinate for click (ydotool only)"
                },
                "y": {
                    "type": "integer",
                    "description": "Y coordinate for click (ydotool only)"
                },
                "button": {
                    "type": "integer",
                    "description": "Mouse button (1=left, 2=middle, 3=right)",
                    "default": 1
                },
                "text": {
                    "type": "string",
                    "description": "Text to type"
                },
                "direction": {
                    "type": "string",
                    "description": "Scroll direction",
                    "enum": ["up", "down", "left", "right"]
                },
                "amount": {
                    "type": "integer",
                    "description": "Scroll amount",
                    "default": 3
                },
                "from_x": {
                    "type": "integer",
                    "description": "Start X for drag"
                },
                "from_y": {
                    "type": "integer",
                    "description": "Start Y for drag"
                },
                "to_x": {
                    "type": "integer",
                    "description": "End X for drag"
                },
                "to_y": {
                    "type": "integer",
                    "description": "End Y for drag"
                },
                "name": {
                    "type": "string",
                    "description": "Window / process name for close_window"
                },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Keys to press (e.g. ['ctrl', 'c'] for ydotool, ['C-c'] for wtype)"
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
        let tool = match Self::find_input_tool().await {
            Some(t) => t,
            None => {
                return Ok(ToolExecutionResult::error(
                    "No input tool found. Install ydotool or wtype.".to_string(),
                ));
            }
        };

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("click");
        info!("Wayland desktop control action: {} (via {})", action, tool);

        match action {
            "click" => {
                let x = args.get("x").and_then(|v| v.as_i64());
                let y = args.get("y").and_then(|v| v.as_i64());
                let button = args.get("button").and_then(|v| v.as_u64()).unwrap_or(1);

                if tool == "ydotool" {
                    if let (Some(xv), Some(yv)) = (x, y) {
                        let _ =
                            Self::run_cmd("ydotool", &["mousemove", &format!("{}, {}", xv, yv)])
                                .await?;
                    }
                    let btn_str = match button {
                        2 => "--middle",
                        3 => "--right",
                        _ => "--left",
                    };
                    let (ok, _, err) = Self::run_cmd("ydotool", &["click", btn_str]).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(format!("Clicked button {}", button)))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Click failed: {}", err)))
                    }
                } else {
                    Ok(ToolExecutionResult::error(
                        "wtype does not support mouse clicks. Install ydotool for click support."
                            .to_string(),
                    ))
                }
            }
            "double_click" => {
                let x = args.get("x").and_then(|v| v.as_i64());
                let y = args.get("y").and_then(|v| v.as_i64());
                let button = args.get("button").and_then(|v| v.as_u64()).unwrap_or(1);

                if tool == "ydotool" {
                    if let (Some(xv), Some(yv)) = (x, y) {
                        let _ =
                            Self::run_cmd("ydotool", &["mousemove", &format!("{}, {}", xv, yv)])
                                .await?;
                    }
                    let (ok, _, _) = Self::run_cmd(
                        "ydotool",
                        &["click", "--repeat", "2", &format!("{}", button)],
                    )
                    .await?;
                    if ok {
                        return Ok(ToolExecutionResult::success(format!(
                            "Double-clicked button {}",
                            button
                        )));
                    }
                    // Fallback: two separate clicks
                    let (ok1, _, _) =
                        Self::run_cmd("ydotool", &["click", &format!("{}", button)]).await?;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let (ok2, _, err2) =
                        Self::run_cmd("ydotool", &["click", &format!("{}", button)]).await?;
                    if ok1 && ok2 {
                        Ok(ToolExecutionResult::success(format!(
                            "Double-clicked button {}",
                            button
                        )))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Double-click failed: {}", err2)))
                    }
                } else {
                    Ok(ToolExecutionResult::error(
                        "wtype does not support mouse clicks".to_string(),
                    ))
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

                if tool == "ydotool" {
                    if let (Some(xv), Some(yv)) = (x, y) {
                        let _ =
                            Self::run_cmd("ydotool", &["mousemove", &format!("{}, {}", xv, yv)])
                                .await?;
                    }
                    let key = match direction {
                        "up" => "PageUp",
                        "down" => "PageDown",
                        "left" => "Left",
                        "right" => "Right",
                        _ => "PageDown",
                    };
                    for _ in 0..amount {
                        let _ = Self::run_cmd("ydotool", &["key", key]).await?;
                    }
                } else {
                    let wtype_key = match direction {
                        "up" => "Page_Up",
                        "down" => "Page_Down",
                        "left" => "Left",
                        "right" => "Right",
                        _ => "Page_Down",
                    };
                    for _ in 0..amount {
                        let _ = Self::run_cmd("wtype", &[wtype_key]).await?;
                    }
                }

                Ok(ToolExecutionResult::success(format!("Scrolled {} ({})", direction, amount)))
            }
            "drag" => {
                let from_x = args.get("from_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let from_y = args.get("from_y").and_then(|v| v.as_i64()).unwrap_or(0);
                let to_x = args.get("to_x").and_then(|v| v.as_i64()).unwrap_or(0);
                let to_y = args.get("to_y").and_then(|v| v.as_i64()).unwrap_or(0);

                if tool == "ydotool" {
                    let _ = Self::run_cmd(
                        "ydotool",
                        &["mousemove", &format!("{}, {}", from_x, from_y)],
                    )
                    .await?;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let _ =
                        Self::run_cmd("ydotool", &["mousemove", &format!("{}, {}", to_x, to_y)])
                            .await?;
                    Ok(ToolExecutionResult::success(format!(
                        "Dragged from ({}, {}) to ({}, {}). Wayland drag is best-effort.",
                        from_x, from_y, to_x, to_y
                    )))
                } else {
                    Ok(ToolExecutionResult::error("wtype does not support drag".to_string()))
                }
            }
            "close_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    let (ok, _, _) = Self::run_cmd("pkill", &["-f", n]).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(format!("Closed process matching '{}'", n)))
                    } else {
                        let (ok2, _, err2) = Self::run_cmd("killall", &[n]).await?;
                        if ok2 {
                            Ok(ToolExecutionResult::success(format!("Closed process '{}'", n)))
                        } else {
                            Ok(ToolExecutionResult::error(format!("Close failed: {}", err2)))
                        }
                    }
                } else {
                    Ok(ToolExecutionResult::error("Provide 'name' for close_window".to_string()))
                }
            }
            "activate_window" => {
                let name = args.get("name").and_then(|v| v.as_str());
                if let Some(n) = name {
                    // Try wlrctl first (generic Wayland compositor controller)
                    let (ok, _, _) = Self::run_cmd("wlrctl", &["window", "focus", n]).await?;
                    if ok {
                        return Ok(ToolExecutionResult::success(format!(
                            "Activated window '{}' via wlrctl",
                            n
                        )));
                    }
                    // Fallback: use ydotool key combo Alt+Tab repeatedly to cycle windows
                    // This is best-effort; real window activation is compositor-dependent on
                    // Wayland.
                    let (ok2, _, err2) = Self::run_cmd("ydotool", &["key", "Alt+Tab"]).await?;
                    if ok2 {
                        Ok(ToolExecutionResult::success(format!(
                            "Best-effort window activation for '{}'. Wayland compositors may \
                             restrict window management.",
                            n
                        )))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Activate failed: {}", err2)))
                    }
                } else {
                    Ok(ToolExecutionResult::error("Provide 'name' for activate_window".to_string()))
                }
            }
            "type" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");

                if tool == "ydotool" {
                    let (ok, _, err) = Self::run_cmd("ydotool", &["type", text]).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(format!("Typed: {}", text)))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Type failed: {}", err)))
                    }
                } else {
                    let (ok, _, err) = Self::run_cmd("wtype", &[text]).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(format!("Typed: {}", text)))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Type failed: {}", err)))
                    }
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

                if tool == "ydotool" {
                    let key_str = keys.join(" ");
                    let (ok, _, err) = Self::run_cmd("ydotool", &["key", &key_str]).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(format!("Pressed: {}", key_str)))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Key press failed: {}", err)))
                    }
                } else {
                    // wtype uses modifier+key format like "C-c" for ctrl+c
                    let key_str = if keys.len() > 1 {
                        // Convert ["ctrl", "c"] -> "C-c"
                        let modifiers: Vec<&str> = keys[..keys.len() - 1]
                            .iter()
                            .map(|k| match k.as_str() {
                                "ctrl" | "control" => "C",
                                "alt" => "M",
                                "shift" => "S",
                                "super" | "win" | "command" => "W",
                                _ => k.as_str(),
                            })
                            .collect();
                        let last = keys.last().map(|s| s.as_str()).unwrap_or("");
                        format!("{}-{}", modifiers.join(""), last)
                    } else {
                        keys.join("")
                    };
                    let (ok, _, err) = Self::run_cmd("wtype", &[&key_str]).await?;
                    if ok {
                        Ok(ToolExecutionResult::success(format!("Pressed: {}", key_str)))
                    } else {
                        Ok(ToolExecutionResult::error(format!("Key press failed: {}", err)))
                    }
                }
            }
            _ => Ok(ToolExecutionResult::error(format!(
                "Unknown action: {}. Use click, type, or key.",
                action
            ))),
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        std::env::var("WAYLAND_DISPLAY").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_desktop_control_tool_creation() {
        let tool = DesktopControlTool::new();
        assert_eq!(tool.name(), "linux_wayland_desktop_control");
    }
}
