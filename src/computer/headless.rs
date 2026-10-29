//! Headless computer adapter — virtual display for CI/CD and server
//! environments.
//!
//! When no physical display is available, this adapter spins up a virtual
//! framebuffer (Xvfb on Linux) so that desktop automation can still run.
//!
//! ```text
//! Agent → HeadlessComputerAdapter → VirtualDisplay → Xvfb
//!                                     │
//!                                     └── screenshot (x11grab / import)
//!                                     └── click/type (xdotool via DISPLAY)
//! ```

use std::process::Stdio;
use std::time::Duration;

use tracing::warn;

use crate::computer::screenshot_encoder::maybe_encode_screenshot;
use crate::computer::{
    ActionResult, ClickTarget, ComputerAdapter, ComputerError, DesktopAction, MouseButton, Rect,
    Result, Screenshot, ScrollDirection, UiElement, WaitCondition,
};
// ── Virtual Display abstraction ────────────────────────────────────────────

/// A virtual display that can host GUI applications without a physical monitor.
#[async_trait::async_trait]
pub trait VirtualDisplay: Send + Sync {
    /// The DISPLAY value (e.g. ":99") used by X11 clients.
    fn display(&self) -> &str;

    /// Capture a screenshot from the virtual framebuffer.
    async fn capture(&self, region: Option<Rect>) -> Result<Screenshot>;

    /// Gracefully shut down the virtual display.
    async fn shutdown(&mut self) -> Result<()>;
}

// ── Linux Xvfb implementation ──────────────────────────────────────────────

/// Xvfb-backed virtual display.
pub struct XvfbDisplay {
    #[allow(dead_code)]
    display_num: u16,
    display: String,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    child: Option<tokio::process::Child>,
}

impl XvfbDisplay {
    /// Try to start an Xvfb instance.
    ///
    /// Tries display numbers starting from `:99` upward until one is free.
    pub async fn start(width: u32, height: u32) -> Result<Self> {
        for display_num in 99..=199u16 {
            let display = format!(":{}", display_num);
            match Self::try_start(display_num, width, height).await {
                Ok(child) => {
                    // Give Xvfb a moment to initialise.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    return Ok(Self {
                        display_num,
                        display,
                        width,
                        height,
                        child: Some(child),
                    });
                }
                Err(e) => {
                    tracing::debug!("Xvfb on display {} failed to start: {}", display_num, e);
                    continue;
                }
            }
        }
        Err(ComputerError::Other("Could not start Xvfb on any display 99-199".to_string()))
    }

    async fn try_start(
        display_num: u16,
        width: u32,
        height: u32,
    ) -> std::io::Result<tokio::process::Child> {
        tokio::process::Command::new("Xvfb")
            .arg(format!(":{}", display_num))
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}x24", width, height))
            .arg("+extension")
            .arg("RANDR")
            .arg("-ac")
            .arg("-nolisten")
            .arg("tcp")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }

    /// Run a command with the correct DISPLAY environment set.
    async fn run_with_display(
        &self,
        cmd: &str,
        args: &[&str],
    ) -> std::io::Result<std::process::Output> {
        tokio::process::Command::new(cmd)
            .args(args)
            .env("DISPLAY", &self.display)
            .output()
            .await
    }
}

#[async_trait::async_trait]
impl VirtualDisplay for XvfbDisplay {
    fn display(&self) -> &str {
        &self.display
    }

    async fn capture(&self, region: Option<Rect>) -> Result<Screenshot> {
        // Strategy 1: ImageMagick import (best quality / simplest)
        let mut import_cmd = tokio::process::Command::new("import");
        import_cmd
            .env("DISPLAY", &self.display)
            .arg("-window")
            .arg("root")
            .arg("png:-");

        if let Some(r) = region {
            import_cmd
                .arg("-crop")
                .arg(format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y));
        }

        match import_cmd.output().await {
            Ok(output) if output.status.success() => {
                let bytes = output.stdout;
                // Write to workspace files dir and apply ScreenshotEncoder
                let screenshot_dir = crate::dirs::workspace_data_dir().join("files");
                let _ = tokio::fs::create_dir_all(&screenshot_dir).await;
                let temp_path =
                    screenshot_dir.join(format!("xvfb_{}.png", crate::utils::ms_timestamp()));
                if let Err(e) = tokio::fs::write(&temp_path, &bytes).await {
                    tracing::warn!("Failed to write temp file '{}': {}", temp_path.display(), e);
                }
                let encoded = maybe_encode_screenshot(&temp_path).await;
                let final_bytes = tokio::fs::read(&encoded).await.unwrap_or(bytes);
                // Cleanup temps
                if let Err(e) = tokio::fs::remove_file(&temp_path).await {
                    tracing::warn!("Failed to cleanup temp file '{}': {}", temp_path.display(), e);
                }
                if encoded != temp_path {
                    if let Err(e) = tokio::fs::remove_file(&encoded).await {
                        tracing::warn!(
                            "Failed to cleanup temp file '{}': {}",
                            encoded.display(),
                            e
                        );
                    }
                }
                let base64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &final_bytes,
                );
                return Ok(Screenshot::new(base64, self.width, self.height));
            }
            Ok(output) => {
                tracing::warn!(
                    "ImageMagick import failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(e) => {
                tracing::warn!("ImageMagick import not available: {}", e);
            }
        }

        // Strategy 2: xwd + convert (fallback)
        let xwd_output = self
            .run_with_display("xwd", &["-root", "-silent"])
            .await
            .map_err(|e| ComputerError::ScreenshotFailed(format!("xwd failed: {}", e)))?;

        if !xwd_output.status.success() {
            return Err(ComputerError::ScreenshotFailed("xwd capture failed".to_string()));
        }

        let mut child = tokio::process::Command::new("convert")
            .arg("xwd:-")
            .arg("png:-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ComputerError::ScreenshotFailed(format!("convert failed: {}", e)))?;

        {
            use tokio::io::AsyncWriteExt;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&xwd_output.stdout).await.map_err(|e| {
                    ComputerError::ScreenshotFailed(format!("convert stdin: {}", e))
                })?;
            }
        }

        let convert_output = child
            .wait_with_output()
            .await
            .map_err(|e| ComputerError::ScreenshotFailed(format!("convert failed: {}", e)))?;

        if !convert_output.status.success() {
            return Err(ComputerError::ScreenshotFailed("xwd→png conversion failed".to_string()));
        }

        let bytes = convert_output.stdout;
        // Write to workspace files dir and apply ScreenshotEncoder
        let screenshot_dir = crate::dirs::workspace_data_dir().join("files");
        let _ = tokio::fs::create_dir_all(&screenshot_dir).await;
        let temp_path = screenshot_dir.join(format!("syscity_xvfb_{}.png", uuid::Uuid::new_v4()));
        if let Err(e) = tokio::fs::write(&temp_path, &bytes).await {
            tracing::warn!("Failed to write temp file '{}': {}", temp_path.display(), e);
        }
        let encoded = maybe_encode_screenshot(&temp_path).await;
        let final_bytes = tokio::fs::read(&encoded).await.unwrap_or(bytes);
        // Cleanup temps
        if let Err(e) = tokio::fs::remove_file(&temp_path).await {
            tracing::warn!("Failed to cleanup temp file '{}': {}", temp_path.display(), e);
        }
        if encoded != temp_path {
            if let Err(e) = tokio::fs::remove_file(&encoded).await {
                tracing::warn!("Failed to cleanup temp file '{}': {}", encoded.display(), e);
            }
        }

        let base64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &final_bytes);

        Ok(Screenshot {
            base64,
            width: self.width,
            height: self.height,
            file_path: None,
            timestamp: std::time::Instant::now(),
        })
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            if let Err(e) = child.start_kill() {
                warn!("Failed to kill Xvfb: {}", e);
            }
            if let Err(e) = child.wait().await {
                warn!("Failed to wait for Xvfb exit: {}", e);
            }
        }
        Ok(())
    }
}

impl Drop for XvfbDisplay {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

// ── Headless Adapter ───────────────────────────────────────────────────────

/// Headless computer adapter that optionally uses a virtual display.
///
/// When `virtual_display` is `Some`, GUI actions (screenshot, click, type)
/// are forwarded to the virtual framebuffer.  When `None`, only non-GUI
/// actions (shell, wait, file) are supported.
pub struct HeadlessComputerAdapter {
    virtual_display: Option<Box<dyn VirtualDisplay>>,
}

impl Default for HeadlessComputerAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadlessComputerAdapter {
    /// Create a new headless adapter **without** a virtual display.
    pub fn new() -> Self {
        Self { virtual_display: None }
    }

    /// Create a new headless adapter **with** an Xvfb virtual display.
    ///
    /// Only available on Linux.  On other platforms this falls back to
    /// `new()`.
    pub async fn with_xvfb() -> Self {
        #[cfg(target_os = "linux")]
        {
            match XvfbDisplay::start(1920, 1080).await {
                Ok(display) => {
                    let display_name = display.display();
                    tracing::info!("Xvfb virtual display started on {}", display_name);
                    return Self {
                        virtual_display: Some(Box::new(display)),
                    };
                }
                Err(e) => {
                    tracing::warn!("Failed to start Xvfb: {}", e);
                }
            }
        }
        Self::new()
    }

    fn display(&self) -> Option<&str> {
        self.virtual_display.as_ref().map(|d| d.display())
    }
}

#[async_trait::async_trait]
impl ComputerAdapter for HeadlessComputerAdapter {
    async fn screenshot(&self, region: Option<Rect>) -> Result<Screenshot> {
        if let Some(display) = &self.virtual_display {
            display.capture(region).await
        } else {
            Err(ComputerError::NoDisplay)
        }
    }

    async fn display_geometry(&self) -> Result<crate::computer::display::DisplayGeometry> {
        // A virtual framebuffer is not this process's own display, and its
        // size is not exposed here. Reporting the host's monitors would be a
        // confident lie; unknown keeps pixel→point conversion refused.
        Ok(crate::computer::display::DisplayGeometry::default())
    }

    async fn read_ui_tree(&self, _app: Option<&str>) -> Result<Vec<UiElement>> {
        // Accessibility tree is not available in headless mode.
        Ok(Vec::new())
    }

    async fn execute(&self, action: DesktopAction) -> Result<ActionResult> {
        match action {
            DesktopAction::Screenshot { region } => {
                let ss = self.screenshot(region).await?;
                Ok(ActionResult::success("screenshot captured")
                    .with_data(serde_json::to_value(&ss).unwrap_or_default()))
            }
            DesktopAction::Click { target, button } => {
                let display = self.display().ok_or_else(|| ComputerError::NoDisplay)?;
                let (x, y) = match target {
                    ClickTarget::Coordinate(p) => (p.x, p.y),
                    _ => {
                        return Err(ComputerError::Other(
                            "Headless adapter only supports coordinate clicks".to_string(),
                        ))
                    }
                };
                let btn = match button {
                    MouseButton::Left => "1",
                    MouseButton::Middle => "2",
                    MouseButton::Right => "3",
                };
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["mousemove", &x.to_string(), &y.to_string()])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["click", btn])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                Ok(ActionResult::success(format!("Clicked at {}, {}", x, y)))
            }
            DesktopAction::Type { text, .. } => {
                let display = self.display().ok_or_else(|| ComputerError::NoDisplay)?;
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["type", &text])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                Ok(ActionResult::success(format!("Typed: {}", text)))
            }
            DesktopAction::KeyPress { keys } => {
                let display = self.display().ok_or_else(|| ComputerError::NoDisplay)?;
                let key_str = keys
                    .iter()
                    .map(|k| crate::computer::evdev_key_name(k))
                    .collect::<Vec<_>>()
                    .join("+");
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["key", &key_str])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                Ok(ActionResult::success(format!("Pressed: {}", key_str)))
            }
            DesktopAction::ClipboardGet | DesktopAction::ClipboardSet { .. } => {
                // Clipboard requires a real X11/Wayland connection
                Err(ComputerError::NoDisplay)
            }
            DesktopAction::LaunchApp { name, args, wait_for_ready } => {
                let mut cmd = tokio::process::Command::new(&name);
                cmd.args(&args);
                if let Some(d) = self.display() {
                    cmd.env("DISPLAY", d);
                }
                let child = cmd.spawn().map_err(|e| {
                    ComputerError::ToolFailed(format!("Failed to launch {}: {}", name, e))
                })?;
                drop(child);
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
            DesktopAction::Scroll { target, direction, amount } => {
                let display = self.display().ok_or_else(|| ComputerError::NoDisplay)?;
                let (x, y) = match target {
                    ClickTarget::Coordinate(p) => (p.x, p.y),
                    _ => {
                        return Err(ComputerError::Other(
                            "Headless adapter only supports coordinate scrolls".to_string(),
                        ))
                    }
                };
                let dir = match direction {
                    ScrollDirection::Up => "4",
                    ScrollDirection::Down => "5",
                    ScrollDirection::Left => "6",
                    ScrollDirection::Right => "7",
                };
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["mousemove", &x.to_string(), &y.to_string()])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                for _ in 0..amount {
                    let output = tokio::process::Command::new("xdotool")
                        .env("DISPLAY", display)
                        .args(["click", dir])
                        .output()
                        .await
                        .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                    if !output.status.success() {
                        return Err(ComputerError::ToolFailed(
                            String::from_utf8_lossy(&output.stderr).to_string(),
                        ));
                    }
                }
                Ok(ActionResult::success(format!(
                    "Scrolled {:?} {} times at {}, {}",
                    direction, amount, x, y
                )))
            }
            DesktopAction::Drag { from, to } => {
                let display = self.display().ok_or_else(|| ComputerError::NoDisplay)?;
                let (x1, y1) = match from {
                    ClickTarget::Coordinate(p) => (p.x, p.y),
                    _ => {
                        return Err(ComputerError::Other(
                            "Headless adapter only supports coordinate drags".to_string(),
                        ))
                    }
                };
                let (x2, y2) = match to {
                    ClickTarget::Coordinate(p) => (p.x, p.y),
                    _ => {
                        return Err(ComputerError::Other(
                            "Headless adapter only supports coordinate drags".to_string(),
                        ))
                    }
                };
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["mousemove", &x1.to_string(), &y1.to_string()])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["mousedown", "1"])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["mousemove", &x2.to_string(), &y2.to_string()])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                let output = tokio::process::Command::new("xdotool")
                    .env("DISPLAY", display)
                    .args(["mouseup", "1"])
                    .output()
                    .await
                    .map_err(|e| ComputerError::ToolFailed(format!("xdotool: {}", e)))?;
                if !output.status.success() {
                    return Err(ComputerError::ToolFailed(
                        String::from_utf8_lossy(&output.stderr).to_string(),
                    ));
                }
                Ok(ActionResult::success(format!("Dragged from {},{} to {},{}", x1, y1, x2, y2)))
            }
            DesktopAction::ActivateWindow { .. } => Err(ComputerError::Other(
                "Window activation not available in headless mode".to_string(),
            )),
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
            DesktopAction::ListWindows
            | DesktopAction::GetWindowGeometry { .. }
            | DesktopAction::MoveWindow { .. }
            | DesktopAction::ResizeWindow { .. }
            | DesktopAction::MinimizeWindow { .. }
            | DesktopAction::MaximizeWindow { .. } => Err(ComputerError::Other(
                "Window management not available in headless mode".to_string(),
            )),
            DesktopAction::ReadUiTree { app } => {
                let tree = self.read_ui_tree(app.as_deref()).await?;
                Ok(ActionResult::success(serde_json::to_string(&tree).unwrap_or_default()))
            }
            _ => Err(ComputerError::Other("Action not available in headless mode".to_string())),
        }
    }

    async fn wait_for(&self, condition: WaitCondition, timeout: Duration) -> Result<bool> {
        let deadline = std::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(500);

        while std::time::Instant::now() < deadline {
            let matched = match &condition {
                WaitCondition::ProcessRunning { name } => {
                    let output = tokio::process::Command::new("pgrep")
                        .arg(name)
                        .output()
                        .await;
                    matches!(output, Ok(out) if out.status.success())
                }
                WaitCondition::ProcessExited { name } => {
                    let output = tokio::process::Command::new("pgrep")
                        .arg(name)
                        .output()
                        .await;
                    !matches!(output, Ok(out) if out.status.success())
                }
                WaitCondition::FileExists { path } => {
                    tokio::fs::try_exists(path).await.unwrap_or(false)
                }
                WaitCondition::WindowTitleContains { pattern } => {
                    let output = tokio::process::Command::new("xdotool")
                        .args(["search", "--name", pattern])
                        .output()
                        .await;
                    matches!(output, Ok(out) if out.status.success())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_headless_adapter_without_display() {
        let adapter = HeadlessComputerAdapter::new();
        assert!(adapter.display().is_none());
    }
}
