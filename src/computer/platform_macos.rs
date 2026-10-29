//! macOS Computer adapter — wraps macOS CapabilitySet tools.

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::computer::element_index::{parse_element_id, ActionTarget, ResolveError, SnapshotCache};
use crate::computer::{
    ActionEffect, ActionEvidence, ActionResult, AxAction, ClickTarget, ComputerAdapter,
    ComputerError, DesktopAction, Rect, Result, Rung, Screenshot, UiElement, WaitCondition,
};
use crate::tools::ToolRegistry;

/// Extract the raw stdout from an AppleScript tool result.
///
/// The AppleScript tool wraps its output in a JSON `AppleScriptResult` struct
/// (`{ success, output, error }`), so the `output` field of `ToolExecutionResult`
/// contains the serialized JSON rather than the raw AppleScript return value.
/// The actual script stdout is in `data["output"]`.
fn apple_script_output(result: &crate::tools::ToolExecutionResult) -> &str {
    result
        .data
        .as_ref()
        .and_then(|d| d.get("output"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// macOS adapter backed by `macos_screenshot`, `macos_accessibility`,
/// `macos_desktop_control`, and `macos_applescript` tools.
pub struct MacosComputerAdapter {
    registry: Arc<ToolRegistry>,
    /// Trees handed to callers, so an element id can be resolved back to the
    /// element it named. See [`crate::computer::element_index`].
    snapshots: SnapshotCache,
}

impl MacosComputerAdapter {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            snapshots: SnapshotCache::new(8),
        }
    }
}

#[async_trait::async_trait]
impl ComputerAdapter for MacosComputerAdapter {
    async fn screenshot(&self, region: Option<Rect>) -> Result<Screenshot> {
        let args = if let Some(r) = region {
            serde_json::json!({
                "region": { "x": r.x, "y": r.y, "width": r.width, "height": r.height }
            })
        } else {
            serde_json::json!({})
        };

        let result = self
            .registry
            .execute("macos_screenshot", args, &crate::tools::ToolContext::default())
            .await
            .ok_or_else(|| ComputerError::ToolFailed("screenshot tool not found".to_string()))?
            .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;

        if !result.success {
            return Err(ComputerError::ScreenshotFailed(result.error.unwrap_or_default()));
        }

        let data = result.data.as_ref();

        let base64 = data
            .and_then(|d| {
                d.get("image_base64")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();

        let width = data
            .and_then(|d| d.get("width").and_then(|v| v.as_u64()))
            .unwrap_or(0) as u32;

        let height = data
            .and_then(|d| d.get("height").and_then(|v| v.as_u64()))
            .unwrap_or(0) as u32;

        let file_path = data
            .and_then(|d| d.get("file_path").and_then(|v| v.as_str()))
            .map(std::path::PathBuf::from);

        let mut ss = Screenshot::new(base64, width, height);
        ss.file_path = file_path;
        Ok(ss)
    }

    async fn read_ui_tree(&self, app: Option<&str>) -> Result<Vec<UiElement>> {
        let args = if let Some(a) = app {
            serde_json::json!({ "action": "tree", "app": a })
        } else {
            serde_json::json!({ "action": "tree" })
        };

        let result = self
            .registry
            .execute("macos_accessibility", args, &crate::tools::ToolContext::default())
            .await
            .ok_or_else(|| ComputerError::ToolFailed("accessibility tool not found".to_string()))?
            .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;

        if !result.success {
            return Err(ComputerError::AccessibilityDenied);
        }

        let elements: Vec<UiElement> = result
            .data
            .as_ref()
            .and_then(|d| {
                d.get("elements")?
                    .as_array()
                    .map(|arr| arr.iter().filter_map(ui_element_from_macos_json).collect())
            })
            .unwrap_or_default();

        let app_name = result
            .data
            .as_ref()
            .and_then(|d| d.get("app"))
            .and_then(|v| v.as_str())
            .map(String::from);

        // Store the tree before returning it: the ids in the returned elements
        // are the ones a caller can hand back to `execute`.
        Ok(self.snapshots.store(elements, app_name).await.roots)
    }

    async fn execute(&self, action: DesktopAction) -> Result<ActionResult> {
        match action {
            DesktopAction::Screenshot { region } => {
                let _t_exec = std::time::Instant::now();
                let ss = self.screenshot(region).await?;
                info!(
                    "[macOS adapter] screenshot() returned in {:?} (base64_len={}, file_path={:?})",
                    _t_exec.elapsed(),
                    ss.base64.len(),
                    ss.file_path,
                );
                // Don't serialize the full Screenshot (which includes 534KB base64).
                // Return a lightweight reference: file path + dimensions.
                let data = if let Some(ref fp) = ss.file_path {
                    serde_json::json!({
                        "file_path": fp.to_string_lossy().to_string(),
                        "width": ss.width,
                        "height": ss.height,
                    })
                } else {
                    serde_json::json!({
                        "width": ss.width,
                        "height": ss.height,
                    })
                };
                info!("[macOS adapter] Screenshot ActionResult built in {:?}", _t_exec.elapsed());
                Ok(ActionResult::success("screenshot captured").with_data(data))
            }
            DesktopAction::Click { target, button: _ } => {
                let (x, y) = self.resolve_click_point(target).await?;
                let script =
                    format!(r#"tell application "System Events" to click at {{ {}, {} }}"#, x, y);
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::DoubleClick { target, button: _ } => {
                let (x, y) = self.resolve_click_point(target).await?;
                let script = format!(
                    r#"tell application "System Events"
    click at {{ {}, {} }}
    delay 0.05
    click at {{ {}, {} }}
end tell"#,
                    x, y, x, y
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::Scroll { target: _, direction, amount } => {
                let key_code = match direction {
                    crate::computer::ScrollDirection::Up => "116",
                    crate::computer::ScrollDirection::Down => "121",
                    crate::computer::ScrollDirection::Left => "123",
                    crate::computer::ScrollDirection::Right => "124",
                };
                let mut script = String::new();
                for _ in 0..amount {
                    script.push_str(&format!(
                        "tell application \"System Events\" to key code {}\ndelay 0.05\n",
                        key_code
                    ));
                }
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(result.output))
            }
            DesktopAction::Drag { from, to } => {
                let (from_x, from_y) = self.resolve_click_point(from).await?;
                let (to_x, to_y) = self.resolve_click_point(to).await?;
                let script = format!(
                    r#"tell application "System Events"
    click at {{ {}, {} }}
    delay 0.1
    key down option
    click at {{ {}, {} }}
    key up option
end tell"#,
                    from_x, from_y, to_x, to_y
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(result.output))
            }
            DesktopAction::CloseWindow { title_pattern } => {
                let args = serde_json::json!({ "action": "close_window", "app": title_pattern });
                let result = self
                    .registry
                    .execute("macos_desktop_control", args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("desktop control not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(result.output))
            }
            DesktopAction::Type { text, .. } => {
                let args = serde_json::json!({ "action": "type", "text": text });
                let result = self
                    .registry
                    .execute("macos_desktop_control", args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("desktop control not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(result.output))
            }
            DesktopAction::KeyPress { keys } => {
                let args = serde_json::json!({ "action": "key_shortcut", "keys": keys });
                let result = self
                    .registry
                    .execute("macos_desktop_control", args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("desktop control not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(result.output))
            }
            DesktopAction::ClipboardGet => {
                let args = serde_json::json!({ "script": "get the clipboard" });
                let result = self
                    .registry
                    .execute("applescript", args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::ClipboardSet { text } => {
                let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                let script = format!("set the clipboard to \"{}\"", escaped);
                let args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::LaunchApp {
                name,
                args: _app_args,
                wait_for_ready,
            } => {
                let script = format!(r#"tell application "{}" to activate"#, name);
                let apple_args = serde_json::json!({ "script": script });
                self.registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;

                if wait_for_ready {
                    let ready = self
                        .wait_for(
                            WaitCondition::ProcessRunning { name: name.clone() },
                            Duration::from_secs(10),
                        )
                        .await?;
                    if !ready {
                        return Ok(ActionResult::error(format!(
                            "Launched {} but it did not appear within 10s",
                            name
                        )));
                    }
                }
                Ok(ActionResult::success(format!("Launched {}", name)))
            }
            DesktopAction::ActivateWindow { title_pattern } => {
                let script = format!(
                    r#"tell application "System Events"
    set frontmost of (first process whose name contains "{}") to true
end tell"#,
                    title_pattern
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::Wait { milliseconds } => {
                tokio::time::sleep(Duration::from_millis(milliseconds)).await;
                Ok(ActionResult::success(format!("Waited {}ms", milliseconds)))
            }
            DesktopAction::GetSystemStatus => {
                let status = tokio::task::spawn_blocking(|| {
                    let mut monitor = crate::computer::system::SystemMonitor::new();
                    monitor.get_status()
                })
                .await
                .map_err(|e| ComputerError::Other(format!("System monitor failed: {}", e)))?;
                Ok(ActionResult::success("System status retrieved")
                    .with_data(serde_json::to_value(&status).unwrap_or_default()))
            }
            DesktopAction::ListProcesses { filter, limit } => {
                let procs = tokio::task::spawn_blocking(move || {
                    let mut monitor = crate::computer::system::SystemMonitor::new();
                    monitor.list_processes(filter.as_deref(), limit)
                })
                .await
                .map_err(|e| ComputerError::Other(format!("Process list failed: {}", e)))?;
                Ok(ActionResult::success(format!("Found {} processes", procs.len()))
                    .with_data(serde_json::to_value(&procs).unwrap_or_default()))
            }
            DesktopAction::KillProcess { pid, name, force } => {
                let killed_pid = tokio::task::spawn_blocking(move || {
                    let mut monitor = crate::computer::system::SystemMonitor::new();
                    monitor.kill_process(pid, name.as_deref(), force)
                })
                .await
                .map_err(|e| ComputerError::Other(format!("Kill failed: {}", e)))??;
                Ok(ActionResult::success(format!("Killed process {}", killed_pid)))
            }
            DesktopAction::RestartProcess { pid, name, force } => {
                let new_pid = tokio::task::spawn_blocking(move || {
                    let mut monitor = crate::computer::system::SystemMonitor::new();
                    monitor.restart_process(pid, name.as_deref(), force)
                })
                .await
                .map_err(|e| ComputerError::Other(format!("Restart failed: {}", e)))??;
                Ok(ActionResult::success(format!("Process restarted, new PID: {}", new_pid)))
            }
            DesktopAction::SetProcessPriority { pid, name, priority } => {
                let updated_pid = tokio::task::spawn_blocking(move || {
                    let mut monitor = crate::computer::system::SystemMonitor::new();
                    monitor.set_process_priority(pid, name.as_deref(), priority)
                })
                .await
                .map_err(|e| ComputerError::Other(format!("Priority change failed: {}", e)))??;
                Ok(ActionResult::success(format!("Priority set for PID {}", updated_pid)))
            }
            // ── Window management ──────────────────────────────────────────
            DesktopAction::ListWindows => {
                let script = r#"tell application "System Events"
    set output to ""
    set procList to (every process whose visible is true)
    repeat with proc in procList
        set procName to name of proc
        set procPid to unix id of proc
        try
            set winTitles to title of every window of proc
            set winIndex to 1
            repeat with winTitle in winTitles
                set output to output & procName & "|||" & (procPid as string) & "|||" & winTitle & "|||" & (winIndex as string) & "\n"
                set winIndex to winIndex + 1
            end repeat
        end try
    end repeat
    return output
end tell"#;
                let args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;

                let mut windows: Vec<crate::computer::WindowInfo> = Vec::new();
                for line in apple_script_output(&result).lines() {
                    let parts: Vec<&str> = line.splitn(4, "|||").collect();
                    if parts.len() >= 3 {
                        windows.push(crate::computer::WindowInfo {
                            id: parts[1].to_string(),
                            title: parts[2].to_string(),
                            app_name: Some(parts[0].to_string()),
                            pid: parts[1].parse().ok(),
                            bounds: None,
                            minimized: false,
                            maximized: false,
                            focused: false,
                        });
                    }
                }
                Ok(ActionResult::success(format!("Found {} windows", windows.len()))
                    .with_data(serde_json::json!({ "windows": windows })))
            }
            DesktopAction::GetWindowGeometry { title_pattern } => {
                let script = format!(
                    r#"tell application "System Events"
    set procList to (every process whose visible is true)
    repeat with proc in procList
        try
            set winTitles to title of every window of proc
            repeat with winIndex from 1 to count of winTitles
                if item winIndex of winTitles contains "{}" then
                    set {{x, y}} to position of window winIndex of proc
                    set {{w, h}} to size of window winIndex of proc
                    return x & "," & y & "," & w & "," & h
                end if
            end repeat
        end try
    end repeat
    return ""
end tell"#,
                    title_pattern
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                let coords: Vec<i32> = apple_script_output(&result)
                    .trim()
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if coords.len() == 4 {
                    let rect = crate::computer::Rect::new(
                        coords[0],
                        coords[1],
                        coords[2] as u32,
                        coords[3] as u32,
                    );
                    Ok(ActionResult::success(format!("Window geometry: {:?}", rect))
                        .with_data(serde_json::json!({ "bounds": rect })))
                } else {
                    Err(ComputerError::ElementNotFound(title_pattern.clone()))
                }
            }
            DesktopAction::MoveWindow { title_pattern, x, y } => {
                let script = format!(
                    r#"tell application "System Events"
    set procList to (every process whose visible is true)
    repeat with proc in procList
        try
            set winTitles to title of every window of proc
            repeat with winIndex from 1 to count of winTitles
                if item winIndex of winTitles contains "{}" then
                    set position of window winIndex of proc to {{{}, {}}}
                    return "moved"
                end if
            end repeat
        end try
    end repeat
    return "not found"
end tell"#,
                    title_pattern, x, y
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::ResizeWindow { title_pattern, width, height } => {
                let script = format!(
                    r#"tell application "System Events"
    set procList to (every process whose visible is true)
    repeat with proc in procList
        try
            set winTitles to title of every window of proc
            repeat with winIndex from 1 to count of winTitles
                if item winIndex of winTitles contains "{}" then
                    set size of window winIndex of proc to {{{}, {}}}
                    return "resized"
                end if
            end repeat
        end try
    end repeat
    return "not found"
end tell"#,
                    title_pattern, width, height
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::MinimizeWindow { title_pattern } => {
                let script = format!(
                    r#"tell application "System Events"
    set procList to (every process whose visible is true)
    repeat with proc in procList
        try
            set winTitles to title of every window of proc
            repeat with winIndex from 1 to count of winTitles
                if item winIndex of winTitles contains "{}" then
                    set minimized of window winIndex of proc to true
                    return "minimized"
                end if
            end repeat
        end try
    end repeat
    try
        set targetProc to first process whose name contains "{}"
        set minimized of window 1 of targetProc to true
        return "minimized"
    end try
    return "not found"
end tell"#,
                    title_pattern, title_pattern
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::MaximizeWindow { title_pattern } => {
                let script = format!(
                    r#"tell application "System Events"
    set procList to (every process whose visible is true)
    repeat with proc in procList
        try
            set winTitles to title of every window of proc
            repeat with winIndex from 1 to count of winTitles
                if item winIndex of winTitles contains "{}" then
                    set zoomed of window winIndex of proc to true
                    return "zoomed"
                end if
            end repeat
        end try
    end repeat
    try
        set targetProc to first process whose name contains "{}"
        set zoomed of window 1 of targetProc to true
        return "zoomed"
    end try
    return "not found"
end tell"#,
                    title_pattern, title_pattern
                );
                let apple_args = serde_json::json!({ "script": script });
                let result = self
                    .registry
                    .execute("applescript", apple_args, &crate::tools::ToolContext::default())
                    .await
                    .ok_or_else(|| {
                        ComputerError::ToolFailed("applescript tool not found".to_string())
                    })?
                    .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
                Ok(ActionResult::success(apple_script_output(&result).to_string()))
            }
            DesktopAction::ReadUiTree { app } => {
                let tree = self.read_ui_tree(app.as_deref()).await?;
                Ok(ActionResult::success(serde_json::to_string(&tree).unwrap_or_default()))
            }
        }
    }

    async fn resolve_target(&self, target: &ClickTarget) -> Result<ActionTarget> {
        if let ClickTarget::Coordinate(p) = target {
            return Ok(ActionTarget::point(p.x, p.y));
        }

        // Snapshot-path ids must resolve against the snapshot that produced
        // them. Only a platform id or a label may trigger a lazy re-read,
        // because those are meant to be looked up in the current tree.
        let lazy = match target {
            ClickTarget::ElementId(id) => parse_element_id(id).is_none(),
            _ => true,
        };

        let mut resolution = self.snapshots.resolve(target).await;
        if lazy
            && matches!(resolution, Err(ResolveError::NotFound(_)) | Err(ResolveError::NoSnapshot))
        {
            // The caller may act on a label without having read the tree.
            self.read_ui_tree(None).await?;
            resolution = self.snapshots.resolve(target).await;
        }

        resolution.map_err(|e| match e {
            ResolveError::Stale(snapshot) => ComputerError::StaleElement(format!(
                "element id belongs to snapshot {snapshot:08x}, which is no longer cached; \
                 re-read the UI tree and use a fresh id"
            )),
            other => ComputerError::ElementNotFound(other.to_string()),
        })
    }

    async fn perform_ax_action(
        &self,
        target: &ActionTarget,
        action: AxAction,
    ) -> Result<ActionResult> {
        let Some(element) = target.element.as_ref() else {
            return Err(ComputerError::UnsupportedPlatform(
                "an accessibility action needs an element target".to_string(),
            ));
        };
        if element.path.is_empty() {
            return Err(ComputerError::UnsupportedPlatform(
                "element has no addressable path".to_string(),
            ));
        }
        let Some(app) = target.app.clone() else {
            return Err(ComputerError::UnsupportedPlatform(
                "the snapshot did not record which application it came from".to_string(),
            ));
        };

        let action_name = match &action {
            AxAction::Press => "press",
            AxAction::Focus => "focus",
            AxAction::SetValue { .. } => "set_value",
        };
        let mut args = serde_json::json!({
            "action": action_name,
            "app": app,
            "path": element.path,
        });
        if let AxAction::SetValue { value } = &action {
            args["value"] = serde_json::json!(value);
        }

        let result = self
            .registry
            .execute("macos_accessibility", args, &crate::tools::ToolContext::default())
            .await
            .ok_or_else(|| ComputerError::ToolFailed("accessibility tool not found".to_string()))?
            .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;

        if result.success {
            Ok(ActionResult::success(
                result
                    .data
                    .as_ref()
                    .and_then(|d| d.get("result"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("ok")
                    .to_string(),
            )
            .with_evidence(ActionEvidence::new(Rung::AxAction, ActionEffect::Confirmed)))
        } else {
            let message = result
                .data
                .as_ref()
                .and_then(|d| d.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("accessibility action failed")
                .to_string();
            Err(ComputerError::ToolFailed(message))
        }
    }

    /// AppKit accessibility actions reach the app through System Events, which
    /// is the same path regardless of which window is in front.
    fn supports_background_input(&self) -> bool {
        true
    }

    /// Press an element or set a field's value without bringing the app
    /// forward.
    ///
    /// Both go through System Events' accessibility scripting, which acts on
    /// the element itself: no key event reaches the event tap, and the app is
    /// never activated. Coordinates are refused on purpose — `click at {x, y}`
    /// is an accessibility hit test at a screen point, so if another window is
    /// in front at that point, the click lands in *that* window.
    async fn perform_background(
        &self,
        action: &DesktopAction,
        target: Option<&ActionTarget>,
    ) -> Result<ActionResult> {
        let Some(target) = target else {
            return Err(ComputerError::BackgroundInputUnavailable(
                "this action names no element, so there is nothing to aim at in the background"
                    .to_string(),
            ));
        };
        let Some(app) = target.app.as_deref() else {
            return Err(ComputerError::BackgroundInputUnavailable(
                "the target's application is unknown; a background action has to name the app it \
                 acts on"
                    .to_string(),
            ));
        };
        let Some(element) = target.element.as_ref() else {
            return Err(ComputerError::BackgroundInputUnavailable(
                "a screen point cannot be clicked in the background — target an element id"
                    .to_string(),
            ));
        };
        let Some(name) = element
            .label
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return Err(ComputerError::BackgroundInputUnavailable(
                "the element has no accessibility name, so a background action cannot name it"
                    .to_string(),
            ));
        };

        let script = match action {
            // A double click has no accessibility equivalent: pressing the
            // element twice is two activations, not one double click.
            DesktopAction::Click { .. } => background_press_script(app, &element.role, name),
            DesktopAction::Type { text, .. } => {
                background_set_value_script(app, &element.role, name, text)
            }
            other => {
                return Err(ComputerError::BackgroundInputUnavailable(format!(
                    "{} has no background form on macOS — an element press or a field value does",
                    desktop_action_name(other)
                )))
            }
        };

        let apple_args = serde_json::json!({ "script": script });
        let result = self
            .registry
            .execute("applescript", apple_args, &crate::tools::ToolContext::default())
            .await
            .ok_or_else(|| ComputerError::ToolFailed("applescript tool not found".to_string()))?
            .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
        Ok(ActionResult::success(apple_script_output(&result).to_string()))
    }

    async fn wait_for(&self, condition: WaitCondition, timeout: Duration) -> Result<bool> {
        let deadline = std::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(500);

        while std::time::Instant::now() < deadline {
            let matched = match &condition {
                WaitCondition::UiTreeContains { role, label } => {
                    let tree = self.read_ui_tree(None).await?;
                    tree.iter().any(|e| {
                        e.role == *role
                            && label
                                .as_ref()
                                .map(|l| e.label.as_ref().map(|el| el.contains(l)).unwrap_or(false))
                                .unwrap_or(true)
                    })
                }
                WaitCondition::WindowTitleContains { pattern } => {
                    let script = r#"tell application "System Events" to return name of first process whose frontmost is true"#;
                    let args = serde_json::json!({ "script": script });
                    if let Some(Ok(result)) = self
                        .registry
                        .execute("applescript", args, &crate::tools::ToolContext::default())
                        .await
                    {
                        apple_script_output(&result).contains(pattern)
                    } else {
                        false
                    }
                }
                WaitCondition::ProcessRunning { name } => {
                    let script = format!(
                        r#"tell application "System Events" to return (name of processes) contains "{}""#,
                        name
                    );
                    let args = serde_json::json!({ "script": script });
                    if let Some(Ok(result)) = self
                        .registry
                        .execute("applescript", args, &crate::tools::ToolContext::default())
                        .await
                    {
                        apple_script_output(&result) == "true"
                    } else {
                        false
                    }
                }
                WaitCondition::FileExists { path } => {
                    tokio::fs::try_exists(path).await.unwrap_or(false)
                }
                _ => false,
            };

            if matched {
                return Ok(true);
            }
            tokio::time::sleep(poll_interval).await;
        }
        Ok(false)
    }
}

impl MacosComputerAdapter {
    /// Resolve a target and return only its coordinates.
    async fn resolve_click_point(&self, target: ClickTarget) -> Result<(i32, i32)> {
        let resolved = self.resolve_target(&target).await?;
        Ok((resolved.x, resolved.y))
    }
}

// ── macOS ⇢ canonical UiElement conversion ────────────────────────────────

/// Parse macOS accessibility JSON (with `position`/`size` strings) into
/// the canonical `UiElement` (with `bounds: Rect`).
///
/// The macOS accessibility tool produces `{ role, name, position: "{x,y}",
/// size: "{w,h}", enabled, value, children }`. The canonical type expects
/// `{ bounds: { x, y, width, height }, label, ... }`.
fn ui_element_from_macos_json(value: &serde_json::Value) -> Option<UiElement> {
    let obj = value.as_object()?;
    let role = obj.get("role")?.as_str()?.to_string();
    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let position = obj.get("position").and_then(|v| v.as_str()).unwrap_or("");
    let size = obj.get("size").and_then(|v| v.as_str()).unwrap_or("");
    let enabled = obj.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let value = obj.get("value").and_then(|v| v.as_str()).map(String::from);

    let children = obj
        .get("children")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(ui_element_from_macos_json).collect())
        .unwrap_or_default();

    Some(UiElement {
        // AXIdentifier, when the application publishes one; the snapshot
        // cache fills in a path id for everything else.
        id: obj
            .get("identifier")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        role,
        label: if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        },
        value,
        bounds: parse_position_size(position, size),
        enabled,
        focused: false,
        children,
    })
}

/// Parse macOS `position` / `size` strings like `"{100, 200}"` / `"{80, 30}"`.
fn parse_position_size(position: &str, size: &str) -> Rect {
    let x = position
        .trim_start_matches('{')
        .split(',')
        .next()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);
    let y = position
        .split(',')
        .nth(1)
        .and_then(|s| s.trim_end_matches('}').trim().parse::<i32>().ok())
        .unwrap_or(0);
    let width = size
        .trim_start_matches('{')
        .split(',')
        .next()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let height = size
        .split(',')
        .nth(1)
        .and_then(|s| s.trim_end_matches('}').trim().parse::<u32>().ok())
        .unwrap_or(0);
    Rect::new(x, y, width, height)
}

/// Escape a string for an AppleScript double-quoted literal.
fn apple_script_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Press an element in an app that is not in front.
///
/// The script deliberately does not activate the app and does not touch the
/// pointer: System Events performs the element's own action.
fn background_press_script(app: &str, role: &str, name: &str) -> String {
    format!(
        r#"tell application "System Events"
    tell application process "{}"
        click (first UI element whose role is "{}" and name is "{}")
    end tell
end tell"#,
        apple_script_escape(app),
        apple_script_escape(role),
        apple_script_escape(name)
    )
}

/// Set a field's value in an app that is not in front.
///
/// `set value` is the accessibility value, so no keystroke is delivered and
/// the field is not focused — focusing would move the selection in an app the
/// person may be reading.
fn background_set_value_script(app: &str, role: &str, name: &str, text: &str) -> String {
    format!(
        r#"tell application "System Events"
    tell application process "{}"
        set value of (first UI element whose role is "{}" and name is "{}") to "{}"
    end tell
end tell"#,
        apple_script_escape(app),
        apple_script_escape(role),
        apple_script_escape(name),
        apple_script_escape(text)
    )
}

/// A short name for an action, for refusal messages.
fn desktop_action_name(action: &DesktopAction) -> &'static str {
    match action {
        DesktopAction::Screenshot { .. } => "a screenshot",
        DesktopAction::Click { .. } => "a click",
        DesktopAction::DoubleClick { .. } => "a double click",
        DesktopAction::Type { .. } => "typing",
        DesktopAction::KeyPress { .. } => "a key press",
        DesktopAction::Scroll { .. } => "scrolling",
        DesktopAction::Drag { .. } => "a drag",
        _ => "this action",
    }
}

/// Factory for macOS.
pub async fn create(registry: Arc<ToolRegistry>) -> Result<Box<dyn ComputerAdapter>> {
    Ok(Box::new(MacosComputerAdapter::new(registry)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_press_never_activates_the_app() {
        let script = background_press_script("Notes", "AXButton", "New Note");
        assert!(
            !script.contains("activate"),
            "activating the app is what takes the foreground: {script}"
        );
        assert!(
            script.contains(r#"first UI element whose role is "AXButton" and name is "New Note""#)
        );
    }

    #[test]
    fn a_background_value_contains_no_keystroke() {
        let script = background_set_value_script("Notes", "AXTextField", "Title", "Hello");
        assert!(!script.contains("activate"), "{script}");
        assert!(!script.contains("keystroke"), "{script}");
        assert!(script.contains(r#"set value of (first UI element whose role is "AXTextField" and name is "Title") to "Hello""#));
    }

    #[test]
    fn quotes_in_names_do_not_end_the_literal() {
        // A field called `He said "hi"` would otherwise close the string and
        // turn the rest of the name into script.
        let script = background_set_value_script("App", "AXTextField", r#"He said "hi""#, "a");
        assert!(script.contains(r#"name is "He said \"hi\"""#), "{script}");
    }
}
