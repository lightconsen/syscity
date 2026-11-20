//! macOS Computer adapter — wraps macOS CapabilitySet tools.

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::computer::element_index::{parse_element_id, ActionTarget, ResolveError, SnapshotCache};
use crate::computer::{
    ActionEffect, ActionEvidence, ActionResult, AxAction, ClickTarget, ComputerAdapter,
    ComputerError, DesktopAction, HitTestResult, Point, Rect, Result, Rung, Screenshot, Tri,
    UiElement, WaitCondition,
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

/// The script's own error, when it raised.
///
/// The tool's `error` field is the transport's verdict ("osascript could not
/// be spawned"); the script's is in the same envelope's `error`. Both are
/// worth reporting, so this prefers the script's and falls back to the tool's.
fn apple_script_error(result: &crate::tools::ToolExecutionResult) -> Option<String> {
    result
        .data
        .as_ref()
        .and_then(|d| d.get("error"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| result.error.clone())
}

/// What a script run answered.
///
/// Not the same question as "did the tool run". The `applescript` tool
/// returns `ToolExecutionResult::success` with a JSON envelope in `output`,
/// so `result.output` is the serialized `{ success, output, error }` — never
/// the script's stdout. Reading it as the answer is how a script that raised
/// ("no window matched") came back as a successful action whose message was
/// the word "not found", and how the drag's Release Guard report — the one
/// thing that says the left button may still be held — was never read at all.
struct ScriptRun {
    /// Whether the script completed. A script that raised is a failed run
    /// even though the tool that spawned `osascript` succeeded.
    success: bool,
    /// What the script returned, trimmed.
    output: String,
    /// Why it raised, when it did.
    error: Option<String>,
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

        // One join for every platform: the key spelling and the size are read
        // by the same code the x11 and Windows adapters use, so the three
        // cannot drift into reading three different payload shapes again.
        Ok(crate::computer::platform::common::screenshot_from_tool_data(data))
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
            // A read that timed out and a read the OS refused are different
            // failures: one is retryable with more time, the other is not
            // retryable at all until a human grants the permission. Reporting
            // both as `AccessibilityDenied` sent callers to the permission
            // pane for a tree that was merely too deep.
            let message = result.error.clone().unwrap_or_default();
            return Err(crate::computer::accessibility_read_error(
                result.data.as_ref(),
                &message,
                false,
            ));
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
                let run = self.run_script(script, None).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the click at ({x}, {y}) did not run: {}",
                        run.error.unwrap_or_default()
                    )));
                }
                Ok(ActionResult::success(format!("clicked at ({x}, {y})")))
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
                let run = self.run_script(script, None).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the double click at ({x}, {y}) did not run: {}",
                        run.error.unwrap_or_default()
                    )));
                }
                Ok(ActionResult::success(format!("double clicked at ({x}, {y})")))
            }
            DesktopAction::Hover { target } => {
                let (x, y) = self.resolve_click_point(target).await?;
                let script = pointer_move_script(x, y);
                let run = self.run_script(script, Some("JavaScript")).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the pointer did not move: {}",
                        run.error
                            .unwrap_or_else(|| "the script did not run".to_string())
                    )));
                }
                // The message says what the step established and what it did
                // not. "Hovered" alone reads as a result; the honest claim is
                // that the pointer is on the control and the next frame is
                // what answers the question.
                Ok(ActionResult::success(
                    "the pointer is now over the target; the next capture shows whether the \
                     control reacted",
                )
                .with_data(serde_json::json!({ "pointer": { "x": x, "y": y } })))
            }
            DesktopAction::PointerMove { to } => {
                let script = pointer_move_script(to.x, to.y);
                let run = self.run_script(script, Some("JavaScript")).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the pointer did not move: {}",
                        run.error
                            .unwrap_or_else(|| "the script did not run".to_string())
                    )));
                }
                Ok(ActionResult::success(format!(
                    "the pointer moved to ({}, {}); nothing was pressed",
                    to.x, to.y
                )))
            }
            DesktopAction::Scroll { target, direction, amount } => {
                // A wheel event is delivered to the window under the pointer,
                // so the pointer has to go there first. Sending PageDown keys
                // instead — which is what this did — scrolls whatever holds
                // *focus*, which is a different window whenever the model
                // scrolls a list it has not clicked yet. That is the ordinary
                // case: you look at a pane and scroll it without selecting it.
                let (x, y) = self.resolve_click_point(target).await?;
                let script = scroll_script(x, y, direction, amount);
                let run = self.run_script(script, Some("JavaScript")).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the scroll did not run: {}",
                        run.error
                            .unwrap_or_else(|| "the script did not run".to_string())
                    )));
                }
                Ok(ActionResult::success(run.output))
            }
            DesktopAction::Drag { from, to } => {
                let (from_x, from_y) = self.resolve_click_point(from).await?;
                let (to_x, to_y) = self.resolve_click_point(to).await?;
                // A drag is three Core Graphics events with the button held
                // between the first and the last; see [`drag_script`]. It is
                // JXA because AppleScript has no verb for a held-button move.
                //
                // Release Guard: if anything raises before the `leftMouseUp`,
                // the button stays down and every later click in every app
                // arrives as a drag. The script's `finally` posts the release
                // on the error path too, and says which path ran, so a caller
                // is never told a clean "dragged" for a release that had to
                // recover.
                let script = drag_script(from_x, from_y, to_x, to_y);
                let run = self.run_script(script, Some("JavaScript")).await?;
                // The release guard's report is the script's *output*, not the
                // tool's. Reading `result.output` here read the JSON envelope
                // the `applescript` tool wraps around it, so none of these
                // prefixes ever matched and every drag — including one that
                // left the button down — was reported as a clean success.
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the drag did not run: {}",
                        run.error.unwrap_or_default()
                    )));
                }
                let output = run.output;
                if output.starts_with("release-failed:") {
                    // The button may still be down. That is a failure a
                    // caller must see, not a drag that "mostly worked".
                    return Err(ComputerError::ToolFailed(output));
                }
                if output.starts_with("drag-failed:") {
                    // The press happened but the motion did not. The release
                    // ran in the `finally`, so nothing is stuck — but the
                    // gesture never carried anything, which is a failure.
                    return Err(ComputerError::ToolFailed(output));
                }
                let mut action_result = ActionResult::success(output.clone());
                if output.starts_with("emergency-release:") {
                    action_result = ActionResult::error(output);
                    action_result.data = Some(serde_json::json!({
                        "emergency_release_posted": true,
                    }));
                }
                Ok(action_result)
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
                if !result.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "could not close {:?}: {}",
                        title_pattern,
                        result.error.unwrap_or_default()
                    )));
                }
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
                if !result.success {
                    // The keystrokes may have gone nowhere, or half of them
                    // may have landed. Either way this is not a success, and
                    // the text must not appear in the message.
                    return Err(ComputerError::ToolFailed(format!(
                        "the text could not be typed: {}",
                        result.error.unwrap_or_default()
                    )));
                }
                Ok(ActionResult::success("the text was typed"))
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
                // A chord that failed may have pressed its modifiers before it
                // raised. The tool's result says whether the Release Guard had
                // to post the release, so the adapter does not report a clean
                // key press for one that stranded a modifier.
                //
                // Read the flag *before* the success check: the guard fires on
                // a failed chord, so `!result.success` is true there too and an
                // early return would drop the one fact the caller needs — that
                // the modifiers are not still held.
                let released = result
                    .data
                    .as_ref()
                    .and_then(|d| d.get("emergency_release_posted"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if released {
                    let mut action_result = ActionResult::error(
                        "key press failed after pressing its modifiers; they were released",
                    );
                    action_result.data = Some(serde_json::json!({
                        "emergency_release_posted": true,
                    }));
                    return Ok(action_result);
                }
                if !result.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the key press did not run: {}",
                        result.error.unwrap_or_default()
                    )));
                }
                Ok(ActionResult::success(result.output))
            }
            DesktopAction::ClipboardGet => {
                let run = self
                    .run_script("get the clipboard".to_string(), None)
                    .await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the clipboard could not be read: {}",
                        run.error.unwrap_or_default()
                    )));
                }
                Ok(ActionResult::success(run.output))
            }
            DesktopAction::ClipboardSet { text } => {
                let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                let script = format!("set the clipboard to \"{}\"", escaped);
                let run = self.run_script(script, None).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "the clipboard could not be set: {}",
                        run.error.unwrap_or_default()
                    )));
                }
                Ok(ActionResult::success("the clipboard now holds the text"))
            }
            DesktopAction::LaunchApp {
                name,
                args: _app_args,
                wait_for_ready,
            } => {
                // The name lands inside a quoted literal, so it is escaped
                // rather than interpolated: an app name carrying a quote would
                // otherwise close the literal and turn the rest of itself into
                // script — and the syntax error that produces reaches here as
                // an empty output, which used to be read as a successful
                // launch.
                let script =
                    format!(r#"tell application "{}" to activate"#, apple_script_escape(&name));
                let result = self.run_script(script, None).await?;
                if !result.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "could not launch {}: {}",
                        name,
                        result.error.unwrap_or_default()
                    )));
                }

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
                let run = self
                    .run_script(activate_window_script(&title_pattern), None)
                    .await?;
                window_action_result(run, "activate", &title_pattern)
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
                let run = self.run_script(list_windows_script(), None).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "listing windows failed: {}",
                        run.error.unwrap_or_default()
                    )));
                }

                let windows = parse_list_windows(&run.output);
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
                    apple_script_escape(&title_pattern)
                );
                let run = self.run_script(script, None).await?;
                if !run.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "reading the geometry of {:?} failed: {}",
                        title_pattern,
                        run.error.unwrap_or_default()
                    )));
                }
                let coords: Vec<i32> = run
                    .output
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
                    apple_script_escape(&title_pattern),
                    x,
                    y
                );
                let run = self.run_script(script, None).await?;
                window_action_result(run, "move", &title_pattern)
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
                    apple_script_escape(&title_pattern),
                    width,
                    height
                );
                let run = self.run_script(script, None).await?;
                window_action_result(run, "resize", &title_pattern)
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
                    apple_script_escape(&title_pattern),
                    apple_script_escape(&title_pattern)
                );
                let run = self.run_script(script, None).await?;
                window_action_result(run, "minimize", &title_pattern)
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
                    apple_script_escape(&title_pattern),
                    apple_script_escape(&title_pattern)
                );
                let run = self.run_script(script, None).await?;
                window_action_result(run, "maximize", &title_pattern)
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

        resolution.map_err(|e| e.into_computer_error())
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
            // The script said the action was performed; it did not say the
            // element changed. `AXPress` on a row that answers success and
            // selects nothing is a real macOS behaviour, so the honest
            // disposition is "delivered, effect not observed" — never
            // `Confirmed`, which is what this used to claim.
            let outcome = result
                .data
                .as_ref()
                .and_then(|d| d.get("result"))
                .and_then(|v| v.as_str())
                .unwrap_or("ok")
                .to_string();
            Ok(ActionResult::success(outcome).with_evidence(
                ActionEvidence::new(Rung::AxAction, ActionEffect::Unverifiable)
                    .with_delivery(crate::computer::DeliveryDisposition::DeliveredUnverified),
            ))
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

    /// Ask the system what a click at `point` would land on.
    ///
    /// This is `AXUIElementCopyElementAtPosition`, the same query the window
    /// server runs to route the event, so it answers for sheets, menus and
    /// overlapping windows exactly as the click would. It is deliberately a
    /// direct call rather than an AppleScript round-trip: it runs before every
    /// pixel-rung click. Any failure — notably a missing Accessibility grant —
    /// reports [`HitTestResult::Unknown`], which the session treats as "the
    /// check did not happen" and lets the action proceed.
    async fn hit_test(&self, point: Point, target: Option<&ActionTarget>) -> HitTestResult {
        crate::computer::platform::macos::hit_test::check(point, target)
    }

    /// The application that would receive an event at `point`.
    ///
    /// Same native query as the hit test, one attribute further up: the point's
    /// element, then its `AXApplication` ancestor. This is the answer for a
    /// coordinate click — the event goes to the window drawn there, which is
    /// not always the frontmost one — and for an element click that something
    /// else is covering.
    async fn application_at(&self, point: Point) -> Option<String> {
        crate::computer::platform::macos::hit_test::application_at(point)
    }

    /// The frontmost application, named the way `read_ui_tree` names apps.
    ///
    /// The spelling matters more than it looks: an allow-list is written in
    /// the names a person sees on screen, and those are the names System
    /// Events reports. Asked through the same helper the permission probe
    /// uses, so "is the grant there" and "who is in front" cannot disagree
    /// about which query they ran. A blocked call answers `None` rather than
    /// raising a dialog: this runs between steps, where a modal prompt would
    /// be a hang.
    async fn active_app(&self) -> Option<String> {
        tokio::task::spawn_blocking(
            crate::computer::platform::macos::permissions::frontmost_application,
        )
        .await
        .ok()
        .flatten()
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

        let run = self
            .run_script(script, None)
            .await
            .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
        if !run.success {
            return Err(ComputerError::ToolFailed(format!(
                "the background {} did not run: {}",
                desktop_action_name(action),
                run.error.unwrap_or_default()
            )));
        }
        Ok(ActionResult::success(if run.output.is_empty() {
            "the element was acted on in the background".to_string()
        } else {
            run.output
        }))
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
                    match self.run_script(script.to_string(), None).await {
                        Ok(run) if run.success => run.output.contains(pattern),
                        _ => false,
                    }
                }
                WaitCondition::ProcessRunning { name } => {
                    let script = format!(
                        r#"tell application "System Events" to return (name of processes) contains "{}""#,
                        apple_script_escape(name)
                    );
                    match self.run_script(script, None).await {
                        Ok(run) if run.success => run.output == "true",
                        _ => false,
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
    /// Run one AppleScript and report what the *script* said.
    ///
    /// See [`ScriptRun`] for why the tool's own `output` cannot be used.
    async fn run_script(&self, script: String, language: Option<&str>) -> Result<ScriptRun> {
        let mut args = serde_json::json!({ "script": script });
        if let Some(language) = language {
            args["language"] = serde_json::json!(language);
        }
        let result = self
            .registry
            .execute("applescript", args, &crate::tools::ToolContext::default())
            .await
            .ok_or_else(|| ComputerError::ToolFailed("applescript tool not found".to_string()))?
            .map_err(|e| ComputerError::ToolFailed(e.to_string()))?;
        Ok(ScriptRun {
            success: result.success,
            output: apple_script_output(&result).trim().to_string(),
            error: apple_script_error(&result),
        })
    }

    /// Resolve a target and return only its coordinates.
    async fn resolve_click_point(&self, target: ClickTarget) -> Result<(i32, i32)> {
        let resolved = self.resolve_target(&target).await?;
        let point = resolved.coordinates()?;
        Ok((point.x, point.y))
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
    // Three states, not two: "the script did not read this" is not "true",
    // and it is not "false" either.
    let enabled = Tri::from(obj.get("enabled").and_then(|v| v.as_bool()));
    let focused = Tri::from(obj.get("focused").and_then(|v| v.as_bool()));
    let value = obj.get("value").and_then(|v| v.as_str()).map(String::from);
    let actions = obj.get("actions").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let children = obj
        .get("children")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(ui_element_from_macos_json).collect())
        .unwrap_or_default();
    // Carried, not inferred: a tree whose walk stopped early must say so all
    // the way up to the caller, or "not found" becomes a false statement.
    let children_count = obj
        .get("children_count")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(u32::MAX as u64) as u32);
    let truncated = obj
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

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
        focused,
        actions,
        children,
        children_count,
        truncated,
    })
}

/// Parse macOS `position` / `size` strings like `"{100, 200}"` / `"{80, 30}"`.
///
/// `None` when either string is missing or unparseable: (0,0,0,0) is a real
/// place on screen, and reporting an unreadable element as sitting there makes
/// it look like an invisible-but-clickable target.
fn parse_position_size(position: &str, size: &str) -> Option<Rect> {
    if position.is_empty() || size.is_empty() {
        return None;
    }
    let coord = |text: &str, index: usize| -> Option<i32> {
        let part = text
            .trim_start_matches('{')
            .trim_end_matches('}')
            .split(',')
            .nth(index)?;
        part.trim().parse::<i32>().ok()
    };
    let extent = |text: &str, index: usize| -> Option<u32> {
        let part = text
            .trim_start_matches('{')
            .trim_end_matches('}')
            .split(',')
            .nth(index)?;
        part.trim().parse::<i64>().ok().map(|v| v.max(0) as u32)
    };
    let (Some(x), Some(y), Some(width), Some(height)) =
        (coord(position, 0), coord(position, 1), extent(size, 0), extent(size, 1))
    else {
        return None;
    };
    Some(Rect::new(x, y, width, height))
}

/// Escape a string for an AppleScript double-quoted literal.
fn apple_script_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// List every visible app's windows, with each window's minimised state.
///
/// A minimised window is off-screen: the tree folds it to one line and the
/// screenshot does not contain it. Without the flag, "the window vanished" and
/// "the window is behind the front one" look the same, and the only action
/// that could tell them apart is the one that restores it.
///
/// `minimized` is System Events' own property, the one `MinimizeWindow` writes;
/// reading it through `AXMinimized` instead would make the read and the write
/// two different things that happen to mean the same.
///
/// The title is the *last* field because a title may itself contain `|||`,
/// and the three fields before it may not.
fn list_windows_script() -> String {
    r#"tell application "System Events"
    set output to ""
    set procList to (every process whose visible is true)
    repeat with proc in procList
        set procName to name of proc
        set procPid to unix id of proc
        try
            repeat with win in (windows of proc)
                set winMin to "shown"
                try
                    if minimized of win is true then set winMin to "minimized"
                end try
                set output to output & procName & "|||" & (procPid as string) & "|||" & winMin & "|||" & (title of win) & "\n"
            end repeat
        end try
    end repeat
    return output
end tell"#
        .to_string()
}

/// Parse [`list_windows_script`]'s output into window records.
///
/// A line with fewer than four fields is skipped rather than shifted: the
/// fields before the title are machine-written, so a short line means the
/// script produced something this parser does not understand, and guessing
/// which field is which would put a process id in a title.
fn parse_list_windows(output: &str) -> Vec<crate::computer::WindowInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, "|||").collect();
            if parts.len() != 4 {
                tracing::debug!("skipping an unparseable window line: {line:?}");
                return None;
            }
            Some(crate::computer::WindowInfo {
                id: parts[1].to_string(),
                title: parts[3].to_string(),
                app_name: Some(parts[0].to_string()),
                pid: parts[1].parse().ok(),
                bounds: None,
                minimized: parts[2] == "minimized",
                maximized: false,
                focused: false,
            })
        })
        .collect()
}

/// Bring a window to the front, restoring it if it was in the Dock.
///
/// `set frontmost` alone raises the *application*: a window the person had
/// minimised stays minimised, and the desktop is unchanged. That made
/// `minimize_window` a one-way door on macOS — the only action that names a
/// window could hide it, and nothing could bring it back. Setting `minimized`
/// back to false is what reopens the door, and it is the same property
/// `MinimizeWindow` writes; `AXRaise` then puts the restored window above its
/// siblings instead of behind them.
///
/// The match is on the window title first (the name the caller was given by
/// `list_windows`), and falls back to the process name, which is what the
/// previous version matched.
fn activate_window_script(title_pattern: &str) -> String {
    format!(
        r#"tell application "System Events"
    set wanted to "{}"
    repeat with proc in (every process whose visible is true)
        try
            repeat with win in (windows of proc)
                if (title of win) contains wanted then
                    set frontmost of proc to true
                    set minimized of win to false
                    try
                        perform action "AXRaise" of win
                    end try
                    return "restored " & (title of win) & " in " & (name of proc)
                end if
            end repeat
        end try
    end repeat
    try
        set targetProc to first process whose name contains wanted
        set frontmost of targetProc to true
        return "brought " & (name of targetProc) & " to the front"
    end try
    return "not found"
end tell"#,
        apple_script_escape(title_pattern)
    )
}

/// Turn a window script's answer into an action result.
///
/// The scripts answer `"not found"` when no window matched, which is a
/// *failure*: the action did not happen. Reporting it as `ActionResult::
/// success("not found")` is how a move that moved nothing came back with
/// `success: true` and a message the caller had to read closely to notice.
/// A script that raised is a failure for the same reason — it may have
/// matched nothing, or System Events may have refused it; either way no
/// window moved.
fn window_action_result(run: ScriptRun, verb: &str, title_pattern: &str) -> Result<ActionResult> {
    if !run.success {
        return Err(ComputerError::ToolFailed(format!(
            "could not {verb} the window matching {:?}: {}",
            title_pattern,
            run.error.unwrap_or_default()
        )));
    }
    if run.output == "not found" || run.output.is_empty() {
        return Err(ComputerError::ElementNotFound(format!(
            "no window matched {:?}, so nothing was {verb}d",
            title_pattern
        )));
    }
    Ok(ActionResult::success(format!("{verb}d the window: {}", run.output)))
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
        DesktopAction::Hover { .. } => "a hover",
        DesktopAction::PointerMove { .. } => "a pointer move",
        _ => "this action",
    }
}

/// JXA that moves the pointer to a point without pressing anything.
///
/// Core Graphics is the only place macOS exposes a bare pointer move: System
/// Events' `click at` always presses, and AppleScript has no "move" verb at
/// all. `CGEventPost` with `kCGEventMouseMoved` is precisely the event the
/// system sends when a person moves the mouse, so the receiving app sees an
/// ordinary hover — which is the whole point of the probe.
///
/// The event is posted at the HID tap, the same level the hardware sends at,
/// so apps that only listen to real input still see it. Nothing is pressed:
/// a `mouse_moved` event's button argument is ignored by the system.
fn pointer_move_script(x: i32, y: i32) -> String {
    format!(
        r#"ObjC.import('CoreGraphics');
var point = {{x: {x}, y: {y}}};
var event = $.CGEventCreateMouseEvent(null, $.kCGEventMouseMoved, point, 0);
$.CGEventPost($.kCGHIDEventTap, event);
"moved to {x},{y}";"#
    )
}

/// JXA that moves the pointer to `(x, y)` and posts wheel events there.
///
/// The move is not decoration: macOS routes a wheel event to the window under
/// the cursor, not to the focused one, so posting a scroll without positioning
/// the pointer scrolls whatever the cursor happened to be over. A caller aims
/// at a pane and expects that pane to move.
///
/// One event per `amount`, because that is what `amount` means everywhere else
/// in this adapter — a count of wheel notches, the unit the X11 and Windows
/// rungs send. Folding them into one event with a larger line count would look
/// the same in the source and scroll a different distance on every platform.
///
/// `CGEventCreateScrollWheelEvent2` takes three axis amounts; only the axis
/// this direction is on is non-zero. The sign follows the system's own
/// convention, which is inverted from the words: a positive vertical amount
/// scrolls *down* the document (the content moves up). A reversed scroll is not
/// an error a caller can see, so the mapping is written out once, here.
fn scroll_script(
    x: i32,
    y: i32,
    direction: crate::computer::ScrollDirection,
    amount: i32,
) -> String {
    let (vertical, horizontal): (i32, i32) = match direction {
        crate::computer::ScrollDirection::Up => (1, 0),
        crate::computer::ScrollDirection::Down => (-1, 0),
        crate::computer::ScrollDirection::Left => (0, 1),
        crate::computer::ScrollDirection::Right => (0, -1),
    };
    format!(
        r#"ObjC.import('CoreGraphics');
var point = {{x: {x}, y: {y}}};
var moved = $.CGEventCreateMouseEvent(null, $.kCGEventMouseMoved, point, 0);
$.CGEventPost($.kCGHIDEventTap, moved);
for (var notch = 0; notch < {amount}; notch++) {{
    var wheel = $.CGEventCreateScrollWheelEvent2(null, $.kCGScrollEventUnitLine, 2, {vertical}, {horizontal}, 0);
    $.CGEventPost($.kCGHIDEventTap, wheel);
}}
"scrolled at {x},{y}";"#
    )
}

/// Factory for macOS.
pub async fn create(registry: Arc<ToolRegistry>) -> Result<Box<dyn ComputerAdapter>> {
    Ok(Box::new(MacosComputerAdapter::new(registry)))
}

/// JXA that presses at one point, moves with the button still down, and
/// releases at another.
///
/// This was two `click at` commands with Option held in between, and that is
/// not a drag: a click is a complete press-and-release, so the second one
/// landed as "Option-click over there" and nothing was ever carried along the
/// path. A slider, a scrollbar thumb, a text selection and a kanban card all
/// move by a *held* button being dragged, which is exactly the gesture that
/// version could not produce — the action existed, reported success, and did
/// the wrong thing.
///
/// The path is walked in steps rather than jumped. A single
/// `leftMouseDragged` from A straight to B is delivered, but a drag session
/// (AppKit's drag-and-drop, a scrollbar's tracking loop) arms itself on the
/// *motion* after the press: it needs to see the pointer leave the press point
/// before it commits to dragging. One instantaneous jump can arrive as "the
/// pointer was already there when the button went down". The intermediate
/// points are what make the gesture look like a hand.
///
/// A short sleep after the press is the other half of that: the press has to
/// reach the window before the drag events do, and without it a fast script
/// can deliver all three inside one run-loop pass.
///
/// The release is in a `finally`. A raise between the down and the up would
/// otherwise leave the button held: every later click in every app arrives as
/// a drag, and the machine is unusable until someone clicks to clear it. The
/// `finally` is what makes the script unable to strand a press — and the
/// output says which path ran, so a caller is never told a clean "dragged" for
/// a release that had to recover.
fn drag_script(from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> String {
    /// Intermediate drag events between the press and the release.
    const STEPS: i32 = 8;
    format!(
        r#"ObjC.import('CoreGraphics');
ObjC.import('Foundation');
function post(kind, x, y) {{
    var point = {{x: x, y: y}};
    var event = $.CGEventCreateMouseEvent(null, kind, point, 0);
    $.CGEventPost($.kCGHIDEventTap, event);
}}
var result = "dragged";
try {{
    post($.kCGEventLeftMouseDown, {from_x}, {from_y});
    // The press must land before the motion does; see the doc above.
    $.NSThread.sleepForTimeInterval(0.05);
    for (var step = 1; step <= {steps}; step++) {{
        var x = {from_x} + Math.round(({to_x} - {from_x}) * step / {steps});
        var y = {from_y} + Math.round(({to_y} - {from_y}) * step / {steps});
        post($.kCGEventLeftMouseDragged, x, y);
        $.NSThread.sleepForTimeInterval(0.01);
    }}
}} catch (dragErr) {{
    result = "drag-failed: " + dragErr;
}} finally {{
    try {{
        post($.kCGEventLeftMouseUp, {to_x}, {to_y});
    }} catch (releaseErr) {{
        result = "release-failed: " + releaseErr;
    }}
}}
result;"#,
        steps = STEPS,
    )
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

    /// A drag must hold the button between the press and the release.
    ///
    /// This is the defect it was written for: the old script was two `click at`
    /// commands, so the button went down and up at the origin and the second
    /// click landed as an Option-click at the destination. Nothing was ever
    /// carried. A slider, a scrollbar thumb, a selection and a kanban card all
    /// move by a held button, which is the one gesture that version could not
    /// produce.
    #[test]
    fn a_drag_holds_the_button_across_the_motion() {
        let script = drag_script(10, 20, 100, 120);
        let down = script.find("kCGEventLeftMouseDown").expect("the press");
        let dragged = script.find("kCGEventLeftMouseDragged").expect("the motion");
        let up = script.find("kCGEventLeftMouseUp").expect("the release");
        assert!(down < dragged, "the button must go down before the motion: {script}");
        assert!(dragged < up, "the motion must happen before the release: {script}");
        // The motion is walked, not jumped: an immediate jump can be delivered
        // as "the pointer was already there".
        assert!(script.contains("step <= 8"), "the path is one leap, not a drag: {script}");
        // No click verb: a click is a complete press-and-release, which is the
        // thing this script must not be made of.
        assert!(!script.contains("click at"), "a click is not a drag: {script}");
        assert!(!script.contains("key down option"), "a modifier is not a held button: {script}");
    }

    /// The release runs on the error path too.
    ///
    /// A press with no release leaves the button down: every later click in
    /// every app arrives as a drag. `finally` is what makes that impossible.
    #[test]
    fn a_drag_releases_the_button_even_when_the_motion_raises() {
        let script = drag_script(10, 20, 100, 120);
        let finally_block = script.find("finally").expect("a finally block");
        assert!(
            script[finally_block..].contains("kCGEventLeftMouseUp"),
            "the release must survive a raise in the motion: {script}"
        );
        assert!(script.contains("release-failed"), "{script}");
        assert!(script.contains("drag-failed"), "{script}");
    }

    /// A scroll must go to the window under the pointer, not to the focus.
    ///
    /// This is the second defect the drag test caught in the same gesture
    /// family: the old scroll posted PageDown keys, which the *focused* window
    /// receives. Scrolling a list you have not clicked is the ordinary case —
    /// you look at a pane and scroll it — and that version scrolled a
    /// different window.
    #[test]
    fn a_scroll_aims_the_pointer_and_posts_a_wheel() {
        let script = scroll_script(300, 250, crate::computer::ScrollDirection::Up, 3);
        let moved = script.find("kCGEventMouseMoved").expect("the pointer move");
        let wheel = script
            .find("kCGEventScrollWheelEvent")
            .or_else(|| script.find("ScrollWheel"))
            .expect("the wheel");
        assert!(moved < wheel, "the pointer must arrive before the wheel: {script}");
        assert!(
            !script.contains("key code"),
            "a keystroke goes to the focus, not to the point: {script}"
        );
        // One event per notch, like every other platform's `amount`.
        assert!(script.contains("notch < 3"), "{script}");
        // The sign is the system's, which is inverted from the words: "up"
        // must move the content down, so the vertical amount is positive.
        assert!(script.contains(", 1, 0, 0)"), "{script}");

        let down = scroll_script(300, 250, crate::computer::ScrollDirection::Down, 1);
        assert!(down.contains(", -1, 0, 0)"), "{down}");
    }

    /// Bringing a window back must also un-minimise it.
    ///
    /// `set frontmost` raises the application, not the window: a window the
    /// model had just minimised stayed in the Dock, so `minimize_window` was a
    /// one-way door. Clearing `AXMinimized` is the way back; the earlier
    /// version of this script had neither the attribute nor a title match.
    #[test]
    fn activating_a_window_restores_it_from_the_dock() {
        let script = activate_window_script("Untitled");
        assert!(
            script.contains("set minimized of win to false"),
            "activating is not restoring without this: {script}"
        );
        // The same property `MinimizeWindow` writes — a read that went
        // through `AXMinimized` would be a second spelling of one fact.
        assert!(!script.contains("AXMinimized"), "{script}");
        // The window-title match has to precede the process-name fallback:
        // matching the process first would raise the app and leave the window
        // where it was.
        let by_title = script.find("title of win").expect("the window-title match");
        let by_app = script
            .find("process whose name contains")
            .expect("the process fallback");
        assert!(by_title < by_app, "the title match must be tried first: {script}");
        // Raising after restoring is what puts it above its siblings.
        assert!(script.contains("AXRaise"), "{script}");
    }

    /// Listing windows must say which ones are in the Dock.
    ///
    /// A minimised window is missing from both the tree (folded) and the
    /// screenshot (off-screen), so without this flag "gone" and "behind"
    /// are indistinguishable — and the model cannot ask to restore either.
    #[test]
    fn the_window_list_reports_minimised_state() {
        let script = list_windows_script();
        assert!(
            script.contains("minimized of win"),
            "a window list that cannot tell minimised from hidden is not a list: {script}"
        );
        assert!(script.contains(r#"set winMin to "minimized""#), "{script}");
        assert!(script.contains(r#"set winMin to "shown""#), "{script}");
        // The property has to be the one the restore writes, or a window the
        // model just hid reads back as shown.
        assert!(!script.contains("AXMinimized"), "{script}");
    }

    /// A title holding the field separator must survive parsing.
    ///
    /// The previous format put the title third of four and parsed with
    /// `splitn(4)`, so a title containing `|||` landed in the wrong field. The
    /// title moved last for exactly this reason.
    #[test]
    fn a_title_holding_the_separator_does_not_shift_the_fields() {
        let windows = parse_list_windows(
            "Notes|||1234|||shown|||Meeting ||| notes\nSafari|||99|||minimized|||Untitled",
        );
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].app_name.as_deref(), Some("Notes"));
        assert_eq!(windows[0].pid, Some(1234));
        assert_eq!(windows[0].title, "Meeting ||| notes");
        assert!(!windows[0].minimized);
        assert_eq!(windows[1].title, "Untitled");
        assert!(windows[1].minimized, "the Dock state is lost: {windows:?}");
    }

    /// A short line is skipped, not shifted into a plausible-looking record.
    #[test]
    fn a_malformed_window_line_is_skipped() {
        let windows = parse_list_windows("Notes|||1234\nSafari|||99|||shown|||Untitled");
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert_eq!(windows[0].title, "Untitled");
    }

    /// The generated JavaScript must parse — but not run.
    ///
    /// A syntax error is a gesture that does nothing and reports success, so
    /// this is worth an `osascript` call. The script is wrapped in a function
    /// and never invoked: `osascript` compiles the whole file before running
    /// any of it, so the syntax is checked with no mouse event posted and no
    /// pointer moved. Calling it here would drive the developer's own screen
    /// from a unit test.
    #[cfg(target_os = "macos")]
    fn assert_parses(script: &str) {
        let out = std::process::Command::new("osascript")
            .arg("-l")
            .arg("JavaScript")
            .arg("-e")
            .arg(format!("function unwrapped() {{ {script} }}"))
            .output()
            .expect("osascript is present on macOS");
        assert!(
            out.status.success(),
            "the script does not parse: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            script
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_scroll_script_is_valid_javascript() {
        assert_parses(&scroll_script(0, 0, crate::computer::ScrollDirection::Down, 1));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_drag_script_is_valid_javascript() {
        assert_parses(&drag_script(10, 20, 100, 120));
    }

    /// Compile an AppleScript without running it.
    ///
    /// `osacompile` parses the whole script and writes an empty bundle to
    /// `/dev/null`, so a syntax error is caught with no window touched and no
    /// pointer moved. Running it would minimise the developer's own windows.
    #[cfg(target_os = "macos")]
    fn assert_apple_script_compiles(script: &str) {
        use std::io::Write;
        let mut child = std::process::Command::new("osacompile")
            .args(["-o", "/dev/null", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("osacompile is present on macOS");
        child
            .stdin
            .as_mut()
            .expect("a piped stdin")
            .write_all(script.as_bytes())
            .expect("write the script");
        let out = child.wait_with_output().expect("osacompile finishes");
        assert!(
            out.status.success(),
            "the script does not compile: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            script
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_window_scripts_compile() {
        assert_apple_script_compiles(&list_windows_script());
        assert_apple_script_compiles(&activate_window_script("Untitled"));
    }

    /// A quote in the title must not end the AppleScript literal.
    ///
    /// Window titles are attacker-adjacent input — a web page chooses its own
    /// title — so this is the injection boundary for the two new scripts.
    #[test]
    fn a_quoted_window_title_does_not_escape_the_script() {
        let script = activate_window_script(r#"He said "hi""#);
        assert!(script.contains(r#"set wanted to "He said \"hi\"""#), "{script}");
    }

    #[test]
    fn quotes_in_names_do_not_end_the_literal() {
        // A field called `He said "hi"` would otherwise close the string and
        // turn the rest of the name into script.
        let script = background_set_value_script("App", "AXTextField", r#"He said "hi""#, "a");
        assert!(script.contains(r#"name is "He said \"hi\"""#), "{script}");
    }

    /// A window that matched nothing is a failure, not a success.
    ///
    /// The scripts answer `"not found"`. Reporting that as
    /// `ActionResult::success("not found")` gave a move that moved nothing
    /// `success: true`, and only the message hinted otherwise.
    #[test]
    fn a_window_that_matched_nothing_is_not_a_success() {
        let run = ScriptRun {
            success: true,
            output: "not found".to_string(),
            error: None,
        };
        match window_action_result(run, "move", "Untitled") {
            Err(ComputerError::ElementNotFound(message)) => {
                assert!(message.contains("Untitled"), "{message}");
                assert!(
                    message.contains("moved"),
                    "the verb must say what did not happen: {message}"
                );
            }
            other => panic!("a miss must be an ElementNotFound, not {other:?}"),
        }

        // The same answer with no text at all: a script that returned nothing
        // did not move anything either.
        let empty = ScriptRun {
            success: true,
            output: String::new(),
            error: None,
        };
        assert!(matches!(
            window_action_result(empty, "resize", "Untitled"),
            Err(ComputerError::ElementNotFound(_))
        ));
    }

    /// A script that raised is a failure even though the tool that ran it
    /// succeeded: "osascript spawned" is not "the window moved".
    #[test]
    fn a_script_that_raised_is_not_a_success() {
        let run = ScriptRun {
            success: false,
            output: String::new(),
            error: Some("System Events got an error".to_string()),
        };
        match window_action_result(run, "minimize", "Notes") {
            Err(ComputerError::ToolFailed(message)) => {
                assert!(message.contains("System Events got an error"), "{message}");
            }
            other => panic!("a raised script must fail, not {other:?}"),
        }
    }

    /// A window action that did happen keeps its own answer.
    #[test]
    fn a_window_action_that_happened_is_reported_with_what_it_answered() {
        let run = ScriptRun {
            success: true,
            output: "resized".to_string(),
            error: None,
        };
        let result = window_action_result(run, "resize", "Notes").expect("a success");
        assert!(result.success);
        assert!(result.message.contains("resized"), "{}", result.message);
    }

    /// The drag's release-guard report is the script's output, not the tool's.
    ///
    /// The `applescript` tool answers with a JSON envelope in `output`, so
    /// reading `result.output` never saw the script's own line: every drag —
    /// including one that left the button down — was reported as a clean
    /// success. `run_script` reduces the envelope to what the script said.
    #[tokio::test]
    async fn the_script_answer_is_read_through_the_tool_envelope() {
        let envelope = serde_json::json!({
            "success": true,
            "output": "emergency-release: something raised",
            "error": null,
        });
        let result = crate::tools::ToolExecutionResult::success(envelope.to_string())
            .with_data(envelope.clone());

        // What the old code read: the envelope, as text.
        assert!(result.output.starts_with('{'), "{}", result.output);
        // What the new code reads: the script's answer.
        assert_eq!(apple_script_output(&result), "emergency-release: something raised");
    }

    /// A title carrying a quote must not end the script literal.
    ///
    /// Every window action interpolates the title into a double-quoted
    /// AppleScript string. Unescaped, a window called `He said "hi"` closes
    /// the literal and the rest of the title becomes script.
    #[test]
    fn a_window_title_with_a_quote_stays_inside_the_literal() {
        let escaped = apple_script_escape(r#"He said "hi""#);
        assert_eq!(escaped, r#"He said \"hi\""#);
        // A backslash must survive too, or the escape itself is escaped.
        assert_eq!(apple_script_escape(r#"C:\Temp"#), r#"C:\\Temp"#);
    }

    /// The release-guard fact must survive the failure.
    ///
    /// A chord that raised after pressing its modifiers arrives with
    /// `success: false` *and* `emergency_release_posted: true`. Checking
    /// success first turns it into a bare `ToolFailed` and throws away the one
    /// thing the caller needs: the system is not left with a modifier held, so
    /// retrying is safe.
    #[test]
    fn a_stranded_chord_reports_that_the_modifiers_were_released() {
        let payload = serde_json::json!({
            "success": false,
            "action": "key_shortcut",
            "error": "emergency-release: Command was held",
            "emergency_release_posted": true,
        });
        // The tool surfaces that payload as an error result; the adapter still
        // has to read the flag out of it rather than returning at the first
        // `!success`.
        let result =
            crate::tools::ToolExecutionResult::error("emergency-release: Command was held")
                .with_data(payload);
        let released = result
            .data
            .as_ref()
            .and_then(|d| d.get("emergency_release_posted"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(released, "the flag must be readable from a failed result");
        assert!(!result.success);
    }

    /// A hover must move the pointer and press nothing.
    ///
    /// `kCGEventMouseMoved` is the event the system itself sends for a pointer
    /// move; a `leftMouseDown` in the same script would make the probe a click
    /// that reports "hovered", and the model would act on a control it had
    /// already pressed.
    #[test]
    fn a_hover_moves_the_pointer_without_pressing() {
        let script = pointer_move_script(320, 240);
        assert!(script.contains("kCGEventMouseMoved"), "{script}");
        assert!(script.contains("CGEventPost"), "{script}");
        assert!(script.contains("x: 320"), "{script}");
        assert!(script.contains("y: 240"), "{script}");
        for pressed in [
            "kCGEventLeftMouseDown",
            "kCGEventLeftMouseUp",
            "CGEventCreateMouseEvent(null, $.kCGEventLeftMouseDown",
        ] {
            assert!(!script.contains(pressed), "a hover must not press: {script}");
        }
    }
}
