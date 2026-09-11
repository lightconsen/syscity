//! Remote control adapter — control remote physical machines via SSH.
//!
//! This adapter implements [`ComputerAdapter`] for remote hosts, allowing the
//! agent to run the same `ComputerUseLoop` against a machine across the
//! network via SSH (shell commands over `ssh`).
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use syscity::computer::remote_control::{
//!     RemoteControlAdapter, RemoteControlConfig, RemoteProtocol,
//! };
//! use syscity::computer::ComputerAdapter;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = RemoteControlConfig {
//!     host: "192.168.1.100".to_string(),
//!     user: "admin".to_string(),
//!     port: 22,
//!     protocol: RemoteProtocol::Ssh {
//!         key_path: Some("~/.ssh/id_rsa".to_string()),
//!     },
//!     ..Default::default()
//! };
//! let adapter = RemoteControlAdapter::new(config).await?;
//! let screenshot = adapter.screenshot(None).await?;
//! # Ok(())
//! # }
//! ```

use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::computer::screenshot_encoder::maybe_encode_screenshot;
use crate::computer::{
    ActionResult, ClickTarget, ComputerAdapter, ComputerError, DesktopAction, MouseButton, Point,
    Rect, Result, Screenshot, UiElement, WaitCondition,
};
// ── Configuration ──────────────────────────────────────────────────────────

/// Remote access protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProtocol {
    /// SSH-based remote control (commands over shell).
    Ssh {
        /// Path to private key (None = use agent / default keys).
        key_path: Option<String>,
    },
}

impl Default for RemoteProtocol {
    fn default() -> Self {
        RemoteProtocol::Ssh { key_path: None }
    }
}

/// Configuration for a remote control session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteControlConfig {
    /// Hostname or IP address.
    pub host: String,
    /// SSH/RDP username.
    pub user: String,
    /// Port (22 for SSH, 5900 for VNC, 3389 for RDP).
    pub port: u16,
    /// Protocol to use.
    pub protocol: RemoteProtocol,
    /// Remote display for Linux X11 apps (e.g. ":0").
    pub display: Option<String>,
    /// Extra SSH options (e.g. ["-o", "StrictHostKeyChecking=no"]).
    pub ssh_extra_args: Vec<String>,
    /// Connection timeout.
    pub connect_timeout: Duration,
}

impl Default for RemoteControlConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            user: std::env::var("USER").unwrap_or_else(|_| "user".to_string()),
            port: 22,
            protocol: RemoteProtocol::default(),
            display: Some(":0".to_string()),
            ssh_extra_args: Vec::new(),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

// ── Remote OS detection ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteOs {
    Linux,
    Macos,
    Windows,
    Unknown,
}

impl RemoteOs {
    fn from_uname(s: &str) -> Self {
        let lower = s.to_lowercase();
        if lower.contains("linux") {
            RemoteOs::Linux
        } else if lower.contains("darwin") {
            RemoteOs::Macos
        } else if lower.contains("windows") || lower.contains("mingw") {
            RemoteOs::Windows
        } else {
            RemoteOs::Unknown
        }
    }
}

// ── Adapter ────────────────────────────────────────────────────────────────

/// A [`ComputerAdapter`] that operates on a remote host.
///
/// Screenshots and input are forwarded over SSH to the remote machine's
/// native automation tools (`xdotool`, `cliclick`, PowerShell, etc.).
pub struct RemoteControlAdapter {
    config: RemoteControlConfig,
    remote_os: RemoteOs,
    /// Layout root the workspace staging directory resolves against.
    paths: std::sync::Arc<crate::dirs::SyscityPaths>,
}

impl std::fmt::Debug for RemoteControlAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteControlAdapter")
            .field("config", &self.config)
            .field("remote_os", &self.remote_os)
            .finish()
    }
}

impl RemoteControlAdapter {
    /// Create a new remote control adapter.
    ///
    /// Probes the remote host to detect its OS.  Fails if the host is
    /// unreachable or SSH authentication fails.
    pub async fn new(
        config: RemoteControlConfig,
        paths: std::sync::Arc<crate::dirs::SyscityPaths>,
    ) -> Result<Self> {
        let mut adapter = Self {
            config,
            remote_os: RemoteOs::Unknown,
            paths,
        };

        adapter.detect_os().await?;
        info!(
            "RemoteControlAdapter connected to {} (OS: {:?})",
            adapter.config.host, adapter.remote_os
        );
        Ok(adapter)
    }

    /// Create without probing (useful in tests).
    pub fn new_unchecked(
        config: RemoteControlConfig,
        paths: std::sync::Arc<crate::dirs::SyscityPaths>,
    ) -> Self {
        Self {
            config,
            remote_os: RemoteOs::Linux,
            paths,
        }
    }

    // ── OS detection ──────────────────────────────────────────────────────

    async fn detect_os(&mut self) -> Result<()> {
        // Try uname first (Unix-like)
        if let Ok(output) = self.run_remote("uname", &["-s"]).await {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if output.status.success() && !stdout.trim().is_empty() {
                self.remote_os = RemoteOs::from_uname(&stdout);
                return Ok(());
            }
        }

        // Windows fallback: try `ver`
        if let Ok(output) = self.run_remote("cmd", &["/c", "ver"]).await {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if output.status.success() && !stdout.trim().is_empty() {
                self.remote_os = RemoteOs::from_uname(&stdout);
                return Ok(());
            }
        }

        warn!("Could not detect OS of remote host {}", self.config.host);
        self.remote_os = RemoteOs::Unknown;
        Ok(())
    }

    // ── SSH helpers ───────────────────────────────────────────────────────

    /// Build the base SSH command with common flags.
    fn ssh_cmd(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-p")
            .arg(self.config.port.to_string());

        if let RemoteProtocol::Ssh { key_path: Some(ref key) } = self.config.protocol {
            cmd.arg("-i").arg(key);
        }

        for extra in &self.config.ssh_extra_args {
            cmd.arg(extra);
        }

        cmd.arg(format!("{}@{}", self.config.user, self.config.host));
        cmd
    }

    /// Run a command on the remote host via SSH.
    async fn run_remote(&self, cmd: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut ssh = self.ssh_cmd();
        ssh.arg(cmd).args(args);
        if let Some(ref display) = self.config.display {
            ssh.env("DISPLAY", display);
        }
        ssh.output().await
    }

    /// Run a command and return stdout as string.
    async fn run_remote_text(&self, cmd: &str, args: &[&str]) -> Result<String> {
        let output = self.run_remote(cmd, args).await.map_err(|e| {
            ComputerError::Other(format!("SSH command failed on {}: {}", self.config.host, e))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComputerError::Other(format!(
                "Remote command '{}' failed: {}",
                cmd, stderr
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // ── Screenshot helpers per OS ─────────────────────────────────────────

    async fn screenshot_linux(&self, region: Option<Rect>) -> Result<Screenshot> {
        let _args: Vec<String> = Vec::new();

        // Try gnome-screenshot first, then scrot, then import
        let tools = [
            ("gnome-screenshot", vec!["-f", "/dev/stdout"]),
            ("scrot", vec!["-"]),
            ("import", vec!["png:-"]),
        ];

        for (tool, tool_args) in &tools {
            let mut ssh = self.ssh_cmd();
            if let Some(ref display) = self.config.display {
                ssh.env("DISPLAY", display);
            }

            if tool == &"import" {
                if let Some(r) = region {
                    let crop = format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y);
                    ssh.arg(tool).args(tool_args).arg(&crop);
                } else {
                    ssh.arg(tool).args(tool_args);
                }
            } else {
                ssh.arg(tool).args(tool_args);
            }

            ssh.stdout(Stdio::piped());
            ssh.stderr(Stdio::piped());

            let output = ssh.output().await.map_err(|e| {
                ComputerError::ScreenshotFailed(format!(
                    "Failed to run remote screenshot tool {}: {}",
                    tool, e
                ))
            })?;

            if output.status.success() && !output.stdout.is_empty() {
                let raw_bytes = output.stdout;
                // Apply ScreenshotEncoder to reduce payload size over SSH.
                let files_dir = self.paths.workspace_data_dir().join("files");
                let _ = std::fs::create_dir_all(&files_dir);
                let temp_path =
                    files_dir.join(format!("remote_{}.png", crate::utils::ms_timestamp()));
                if let Err(e) = tokio::fs::write(&temp_path, &raw_bytes).await {
                    tracing::warn!("Failed to write temp file '{}': {}", temp_path.display(), e);
                }
                let encoded = maybe_encode_screenshot(&temp_path).await;
                let final_bytes = tokio::fs::read(&encoded).await.unwrap_or(raw_bytes);
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
                #[cfg(feature = "image")]
                let (width, height) = if let Ok(img) = image::load_from_memory(&final_bytes) {
                    (img.width(), img.height())
                } else {
                    (0, 0)
                };
                #[cfg(not(feature = "image"))]
                let (width, height) = (0, 0);
                return Ok(Screenshot::new(base64, width, height));
            }
        }

        Err(ComputerError::ScreenshotFailed(
            "All remote screenshot tools failed on Linux host".to_string(),
        ))
    }

    async fn screenshot_macos(&self, _region: Option<Rect>) -> Result<Screenshot> {
        let mut ssh = self.ssh_cmd();
        ssh.arg("screencapture")
            .arg("-x") // no sound
            .arg("-t")
            .arg("png")
            .arg("-")
            .stdout(Stdio::piped());

        let output = ssh.output().await.map_err(|e| {
            ComputerError::ScreenshotFailed(format!("screencapture failed on remote macOS: {}", e))
        })?;

        if !output.status.success() || output.stdout.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComputerError::ScreenshotFailed(format!(
                "screencapture failed: {}",
                stderr
            )));
        }

        let raw_bytes = output.stdout;
        // Apply ScreenshotEncoder to reduce payload size over SSH.
        let temp_path = self
            .paths
            .workspace_data_dir()
            .join("files")
            .join(format!("remote_{}.png", crate::utils::ms_timestamp()));
        if let Err(e) = tokio::fs::write(&temp_path, &raw_bytes).await {
            tracing::warn!("Failed to write temp file '{}': {}", temp_path.display(), e);
        }
        let encoded = maybe_encode_screenshot(&temp_path).await;
        let final_bytes = tokio::fs::read(&encoded).await.unwrap_or(raw_bytes);
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
        #[cfg(feature = "image")]
        let (width, height) = if let Ok(img) = image::load_from_memory(&final_bytes) {
            (img.width(), img.height())
        } else {
            (0, 0)
        };
        #[cfg(not(feature = "image"))]
        let (width, height) = (0, 0);

        Ok(Screenshot::new(base64, width, height))
    }

    async fn screenshot_windows(&self, _region: Option<Rect>) -> Result<Screenshot> {
        // PowerShell + .NET for screenshot capture
        let ps = r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap($screen.Width, $screen.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($screen.Location, [System.Drawing.Point]::Empty, $screen.Size)
$ms = New-Object System.IO.MemoryStream
$bitmap.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
[System.Convert]::ToBase64String($ms.ToArray())
"#;

        let output = self
            .run_remote("powershell", &["-Command", ps])
            .await
            .map_err(|e| {
                ComputerError::ScreenshotFailed(format!(
                    "PowerShell screenshot failed on remote Windows: {}",
                    e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ComputerError::ScreenshotFailed(format!(
                "PowerShell screenshot failed: {}",
                stderr
            )));
        }

        let b64 = String::from_utf8_lossy(&output.stdout);
        let b64 = b64.trim();

        // Decode → apply ScreenshotEncoder → re-encode (reduces payload over SSH).
        let final_b64 = if let Ok(decoded) =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
        {
            let temp_path = self
                .paths
                .workspace_data_dir()
                .join("files")
                .join(format!("remote_{}.png", crate::utils::ms_timestamp()));
            if let Err(e) = tokio::fs::write(&temp_path, &decoded).await {
                tracing::warn!("Failed to write temp file '{}': {}", temp_path.display(), e);
            }
            let encoded = maybe_encode_screenshot(&temp_path).await;
            let final_bytes = tokio::fs::read(&encoded).await.unwrap_or(decoded.clone());
            if let Err(e) = tokio::fs::remove_file(&temp_path).await {
                tracing::warn!("Failed to cleanup temp file '{}': {}", temp_path.display(), e);
            }
            if encoded != temp_path {
                if let Err(e) = tokio::fs::remove_file(&encoded).await {
                    tracing::warn!("Failed to cleanup temp file '{}': {}", encoded.display(), e);
                }
            }
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &final_bytes)
        } else {
            b64.to_string()
        };

        #[cfg(feature = "image")]
        let (width, height) = {
            if let Ok(bytes) =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &final_b64)
            {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    (img.width(), img.height())
                } else {
                    (0, 0)
                }
            } else {
                (0, 0)
            }
        };
        #[cfg(not(feature = "image"))]
        let (width, height) = (0, 0);

        Ok(Screenshot::new(final_b64, width, height))
    }

    // ── Input helpers ─────────────────────────────────────────────────────

    async fn click_remote(&self, point: Point, button: MouseButton) -> Result<ActionResult> {
        match self.remote_os {
            RemoteOs::Linux => {
                let btn = match button {
                    MouseButton::Left => "1",
                    MouseButton::Middle => "2",
                    MouseButton::Right => "3",
                };
                self.run_remote_text(
                    "xdotool",
                    &[
                        "mousemove",
                        &point.x.to_string(),
                        &point.y.to_string(),
                        "click",
                        btn,
                    ],
                )
                .await?;
            }
            RemoteOs::Macos => {
                let btn = match button {
                    MouseButton::Left => "left",
                    MouseButton::Middle => "middle",
                    MouseButton::Right => "right",
                };
                self.run_remote_text("cliclick", &[&format!("{}:{},{}", btn, point.x, point.y)])
                    .await?;
            }
            RemoteOs::Windows => {
                let ps = format!(
                    r#"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({}, {})"#,
                    point.x, point.y
                );
                self.run_remote_text("powershell", &["-Command", &ps])
                    .await?;
                // TODO: simulate mouse click on Windows
            }
            _ => {
                return Err(ComputerError::UnsupportedPlatform(format!(
                    "Remote click not supported for OS {:?}",
                    self.remote_os
                )))
            }
        }
        Ok(ActionResult::success("clicked"))
    }

    async fn type_remote(&self, text: &str) -> Result<ActionResult> {
        match self.remote_os {
            RemoteOs::Linux => {
                self.run_remote_text("xdotool", &["type", "--delay", "10", text])
                    .await?;
            }
            RemoteOs::Macos => {
                self.run_remote_text(
                    "osascript",
                    &[
                        "-e",
                        &format!("tell application \"System Events\" to keystroke \"{}\"", text),
                    ],
                )
                .await?;
            }
            RemoteOs::Windows => {
                let ps = format!(
                    r#"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{}')"#,
                    text.replace("'", "''")
                );
                self.run_remote_text("powershell", &["-Command", &ps])
                    .await?;
            }
            _ => {
                return Err(ComputerError::UnsupportedPlatform(format!(
                    "Remote type not supported for OS {:?}",
                    self.remote_os
                )))
            }
        }
        Ok(ActionResult::success("typed"))
    }

    async fn keypress_remote(&self, keys: &[String]) -> Result<ActionResult> {
        let joined = keys.join("+");
        match self.remote_os {
            RemoteOs::Linux => {
                self.run_remote_text("xdotool", &["key", &joined]).await?;
            }
            RemoteOs::Macos => {
                // Convert simple keys to AppleScript key codes or use cliclick
                if self
                    .run_remote_text("cliclick", &[&format!("kp:{}", joined)])
                    .await
                    .is_err()
                {
                    self.run_remote_text(
                        "osascript",
                        &[
                            "-e",
                            &format!("tell application \"System Events\" to key code {}", joined),
                        ],
                    )
                    .await?;
                }
            }
            RemoteOs::Windows => {
                let ps = format!(
                    r#"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{}')"#,
                    joined
                );
                self.run_remote_text("powershell", &["-Command", &ps])
                    .await?;
            }
            _ => {
                return Err(ComputerError::UnsupportedPlatform(format!(
                    "Remote keypress not supported for OS {:?}",
                    self.remote_os
                )))
            }
        }
        Ok(ActionResult::success("key pressed"))
    }

    // ── Remote UI tree helpers ────────────────────────────────────────────

    /// Read the UI tree of the remote Linux host via pyatspi (primary) or
    /// wmctrl (fallback).
    async fn read_ui_tree_linux(&self, _app: Option<&str>) -> Result<Vec<UiElement>> {
        // Strategy 1: Use python3 with pyatspi for a rich accessibility tree.
        let py_script = r#"
import sys
try:
    import pyatspi
    desktop = pyatspi.Registry.getDesktop(0)
    def dump(obj, depth=0):
        if depth > 5:
            return
        name = obj.name or ''
        role = obj.getRoleName() or ''
        try:
            ext = obj.queryComponent().getExtents(0)
            x, y, w, h = ext.x, ext.y, ext.width, ext.height
        except:
            x, y, w, h = 0, 0, 0, 0
        enabled = obj.getState().contains(pyatspi.STATE_ENABLED)
        print(f'{x} {y} {w} {h}|{role}|{name}|{1 if enabled else 0}')
        for i in range(obj.childCount):
            dump(obj[i], depth + 1)
    dump(desktop)
except Exception as e:
    print(f'PYATSPI_ERROR:{e}', file=sys.stderr)
"#;
        let output = self.run_remote("python3", &["-c", py_script]).await;
        if let Ok(out) = output {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let mut elements = Vec::new();
                for line in stdout.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = line.splitn(2, '|').collect();
                    if parts.len() == 2 {
                        let coords: Vec<i32> = parts[0]
                            .split_whitespace()
                            .filter_map(|s| s.parse().ok())
                            .collect();
                        let meta: Vec<&str> = parts[1].split('|').collect();
                        if coords.len() == 4 && meta.len() == 3 {
                            elements.push(UiElement {
                                id: String::new(),
                                role: meta[0].trim().to_string(),
                                label: {
                                    let l = meta[1].trim().to_string();
                                    if l.is_empty() {
                                        None
                                    } else {
                                        Some(l)
                                    }
                                },
                                value: None,
                                bounds: Rect::new(
                                    coords[0],
                                    coords[1],
                                    coords[2] as u32,
                                    coords[3] as u32,
                                ),
                                enabled: meta[2].trim() == "1",
                                focused: false,
                                children: vec![],
                            });
                        }
                    }
                }
                if !elements.is_empty() {
                    return Ok(elements);
                }
            }
        }

        // Strategy 2 (fallback): wmctrl for window-level info.
        if let Ok(windows) = self.run_remote_text("wmctrl", &["-l"]).await {
            let mut elements = Vec::new();
            for line in windows.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(4, ' ').collect();
                if parts.len() >= 4 {
                    let title = parts[3].trim();
                    elements.push(UiElement {
                        id: parts[0].to_string(),
                        role: "window".to_string(),
                        label: if title.is_empty() {
                            None
                        } else {
                            Some(title.to_string())
                        },
                        value: None,
                        bounds: Rect::new(0, 0, 0, 0),
                        enabled: true,
                        focused: false,
                        children: vec![],
                    });
                }
            }
            if !elements.is_empty() {
                return Ok(elements);
            }
        }

        warn!("No accessible UI tree tools found on remote Linux (try installing python3-pyatspi)");
        Ok(Vec::new())
    }

    /// Read the UI tree of the remote Windows host via PowerShell UIAutomation.
    async fn read_ui_tree_windows(&self, _app: Option<&str>) -> Result<Vec<UiElement>> {
        let ps_script = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$desktop = [System.Windows.Automation.AutomationElement]::RootElement
function Dump-Tree($element, $depth) {
    if ($depth -gt 5) { return }
    $name = $element.Current.Name
    $role = $element.Current.LocalizedControlType
    $rect = $element.Current.BoundingRectangle
    $enabled = $element.Current.IsEnabled
    if ($rect -ne $null) {
        $x = [int]$rect.X; $y = [int]$rect.Y
        $w = [int]$rect.Width; $h = [int]$rect.Height
        Write-Output "$x $y $w $h |$role|$name|$enabled"
    }
    $walker = New-Object System.Windows.Automation.TreeWalker(
        [System.Windows.Automation.Condition]::TrueCondition)
    $child = $walker.GetFirstChild($element)
    while ($child -ne $null) {
        Dump-Tree $child ($depth + 1)
        $child = $walker.GetNextSibling($child)
    }
}
Dump-Tree $desktop 0
"#;
        if let Ok(output) = self
            .run_remote("powershell", &["-NoProfile", "-Command", ps_script])
            .await
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut elements = Vec::new();
                for line in stdout.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = line.splitn(2, '|').collect();
                    if parts.len() == 2 {
                        let coords: Vec<i32> = parts[0]
                            .split_whitespace()
                            .filter_map(|s| s.parse().ok())
                            .collect();
                        let meta: Vec<&str> = parts[1].split('|').collect();
                        if coords.len() == 4 && meta.len() == 3 {
                            elements.push(UiElement {
                                id: String::new(),
                                role: meta[0].trim().to_string(),
                                label: {
                                    let l = meta[1].trim().to_string();
                                    if l.is_empty() {
                                        None
                                    } else {
                                        Some(l)
                                    }
                                },
                                value: None,
                                bounds: Rect::new(
                                    coords[0],
                                    coords[1],
                                    coords[2] as u32,
                                    coords[3] as u32,
                                ),
                                enabled: meta[2].trim() == "True",
                                focused: false,
                                children: vec![],
                            });
                        }
                    }
                }
                if !elements.is_empty() {
                    return Ok(elements);
                }
            }
        }

        warn!("PowerShell UIAutomation failed on remote Windows (may require .NET Framework)");
        Ok(Vec::new())
    }
}

// ── ComputerAdapter impl ───────────────────────────────────────────────────

#[async_trait::async_trait]
impl ComputerAdapter for RemoteControlAdapter {
    async fn screenshot(&self, region: Option<Rect>) -> Result<Screenshot> {
        match self.remote_os {
            RemoteOs::Linux => self.screenshot_linux(region).await,
            RemoteOs::Macos => self.screenshot_macos(region).await,
            RemoteOs::Windows => self.screenshot_windows(region).await,
            RemoteOs::Unknown => {
                // Try Linux tools as fallback
                self.screenshot_linux(region).await
            }
        }
    }

    async fn read_ui_tree(&self, app: Option<&str>) -> Result<Vec<UiElement>> {
        match self.remote_os {
            RemoteOs::Linux => self.read_ui_tree_linux(app).await,
            RemoteOs::Windows => self.read_ui_tree_windows(app).await,
            RemoteOs::Macos => {
                // macOS AXUIElement accessibility via SSH is impractical.
                warn!("read_ui_tree not supported for remote macOS");
                Ok(Vec::new())
            }
            _ => {
                warn!("read_ui_tree not supported for remote OS {:?}", self.remote_os);
                Ok(Vec::new())
            }
        }
    }

    async fn execute(&self, action: DesktopAction) -> Result<ActionResult> {
        match action {
            DesktopAction::Screenshot { region } => {
                let ss = self.screenshot(region).await?;
                Ok(ActionResult::success("screenshot captured").with_screenshot(ss))
            }
            DesktopAction::Click { target, button } => {
                let point = match target {
                    ClickTarget::Coordinate(p) => p,
                    _ => {
                        return Err(ComputerError::ElementNotFound(
                            "Remote adapter only supports coordinate clicks".to_string(),
                        ));
                    }
                };
                self.click_remote(point, button).await
            }
            DesktopAction::DoubleClick { target, button } => {
                let point = match target {
                    ClickTarget::Coordinate(p) => p,
                    _ => {
                        return Err(ComputerError::ElementNotFound(
                            "Remote adapter only supports coordinate clicks".to_string(),
                        ));
                    }
                };
                self.click_remote(point, button).await?;
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.click_remote(point, button).await
            }
            DesktopAction::Type { text } => self.type_remote(&text).await,
            DesktopAction::KeyPress { keys } => self.keypress_remote(&keys).await,
            DesktopAction::Wait { milliseconds } => {
                tokio::time::sleep(Duration::from_millis(milliseconds)).await;
                Ok(ActionResult::success("waited"))
            }
            DesktopAction::ClipboardGet => {
                let text = match self.remote_os {
                    RemoteOs::Linux => {
                        self.run_remote_text("xclip", &["-o", "-selection", "clipboard"])
                            .await?
                    }
                    RemoteOs::Macos => self.run_remote_text("pbpaste", &[]).await?,
                    RemoteOs::Windows => {
                        let ps = r#"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::GetText()"#;
                        self.run_remote_text("powershell", &["-Command", ps])
                            .await?
                    }
                    _ => {
                        return Err(ComputerError::UnsupportedPlatform(
                            "Clipboard get not supported".to_string(),
                        ))
                    }
                };
                Ok(ActionResult::success(text))
            }
            DesktopAction::ClipboardSet { text } => {
                match self.remote_os {
                    RemoteOs::Linux => {
                        let mut ssh = self.ssh_cmd();
                        ssh.arg("xclip").args(["-i", "-selection", "clipboard"]);
                        ssh.stdin(Stdio::piped());
                        let mut child = ssh
                            .spawn()
                            .map_err(|e| ComputerError::Other(e.to_string()))?;
                        if let Some(mut stdin) = child.stdin.take() {
                            stdin
                                .write_all(text.as_bytes())
                                .await
                                .map_err(|e| ComputerError::Other(e.to_string()))?;
                        }
                        if let Err(e) = child.wait().await {
                            warn!("Failed to wait for remote clipboard (Linux): {}", e);
                        }
                    }
                    RemoteOs::Macos => {
                        let mut ssh = self.ssh_cmd();
                        ssh.arg("pbcopy");
                        ssh.stdin(Stdio::piped());
                        let mut child = ssh
                            .spawn()
                            .map_err(|e| ComputerError::Other(e.to_string()))?;
                        if let Some(mut stdin) = child.stdin.take() {
                            stdin
                                .write_all(text.as_bytes())
                                .await
                                .map_err(|e| ComputerError::Other(e.to_string()))?;
                        }
                        if let Err(e) = child.wait().await {
                            warn!("Failed to wait for remote clipboard (macOS): {}", e);
                        }
                    }
                    RemoteOs::Windows => {
                        let ps = format!(
                            r#"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::SetText('{}')"#,
                            text.replace("'", "''")
                        );
                        self.run_remote_text("powershell", &["-Command", &ps])
                            .await?;
                    }
                    _ => {
                        return Err(ComputerError::UnsupportedPlatform(
                            "Clipboard set not supported".to_string(),
                        ))
                    }
                }
                Ok(ActionResult::success("clipboard set"))
            }
            DesktopAction::LaunchApp { name, args, wait_for_ready } => {
                let mut cmd_args = vec![name];
                cmd_args.extend(args);
                let output = self
                    .run_remote_text(
                        &cmd_args[0],
                        &cmd_args[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    )
                    .await?;
                if wait_for_ready {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Ok(ActionResult::success(output))
            }
            DesktopAction::GetSystemStatus => {
                let output = self.run_remote_text("uname", &["-a"]).await?;
                Ok(ActionResult::success(output))
            }
            DesktopAction::ListProcesses { filter: _, limit } => {
                let args = vec!["aux"];
                if let Some(l) = limit {
                    let output = self
                        .run_remote("ps", &args)
                        .await
                        .map_err(|e| ComputerError::Other(e.to_string()))?;
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let lines: Vec<_> = stdout.lines().take(l + 1).collect();
                    return Ok(ActionResult::success(lines.join("\n")));
                }
                let output = self.run_remote_text("ps", &args).await?;
                Ok(ActionResult::success(output))
            }
            DesktopAction::KillProcess { pid, name, force } => {
                if let Some(pid) = pid {
                    let sig = if force { "-9" } else { "-15" };
                    self.run_remote_text("kill", &[sig, &pid.to_string()])
                        .await?;
                } else if let Some(name) = name {
                    let cmd = if force { "killall" } else { "pkill" };
                    self.run_remote_text(cmd, &[&name]).await?;
                }
                Ok(ActionResult::success("process killed"))
            }
            DesktopAction::ReadUiTree { app } => {
                let tree = self.read_ui_tree(app.as_deref()).await?;
                Ok(ActionResult::success(serde_json::to_string(&tree).unwrap_or_default()))
            }
            _ => {
                warn!("Remote adapter received unsupported action: {:?}", action);
                Err(ComputerError::UnsupportedPlatform(format!(
                    "Action {:?} not supported over remote control",
                    action
                )))
            }
        }
    }

    async fn wait_for(&self, condition: WaitCondition, timeout: Duration) -> Result<bool> {
        let start = std::time::Instant::now();
        let check_interval = Duration::from_millis(500);

        while start.elapsed() < timeout {
            let matched = match &condition {
                WaitCondition::ProcessRunning { name } => {
                    let output = self.run_remote("pgrep", &[name]).await;
                    output.map(|o| o.status.success()).unwrap_or(false)
                }
                WaitCondition::ProcessExited { name } => {
                    let output = self.run_remote("pgrep", &[name]).await;
                    output.map(|o| !o.status.success()).unwrap_or(true)
                }
                WaitCondition::FileExists { path } => {
                    let output = self.run_remote("test", &["-f", path]).await;
                    output.map(|o| o.status.success()).unwrap_or(false)
                }
                WaitCondition::WindowTitleContains { pattern } => {
                    // Use xdotool on Linux to search for a window matching the pattern.
                    if self.remote_os == RemoteOs::Linux {
                        let output = self
                            .run_remote("xdotool", &["search", "--name", pattern])
                            .await;
                        output.map(|o| o.status.success()).unwrap_or(false)
                    } else {
                        warn!(
                            "WindowTitleContains wait not supported on remote {:?}",
                            self.remote_os
                        );
                        false
                    }
                }
                _ => {
                    warn!("wait_for condition {:?} not supported remotely", condition);
                    false
                }
            };

            if matched {
                return Ok(true);
            }
            tokio::time::sleep(check_interval).await;
        }

        Ok(false)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_os_from_uname() {
        assert_eq!(RemoteOs::from_uname("Linux"), RemoteOs::Linux);
        assert_eq!(RemoteOs::from_uname("Darwin"), RemoteOs::Macos);
        assert_eq!(RemoteOs::from_uname("MINGW64_NT-10.0"), RemoteOs::Windows);
        assert_eq!(RemoteOs::from_uname("unknown"), RemoteOs::Unknown);
    }

    #[test]
    fn test_remote_protocol_default() {
        let p = RemoteProtocol::default();
        assert!(matches!(p, RemoteProtocol::Ssh { key_path: None }));
    }

    #[test]
    fn test_remote_control_config_default() {
        let cfg = RemoteControlConfig::default();
        assert_eq!(cfg.port, 22);
        assert_eq!(cfg.host, "localhost");
    }

    #[test]
    fn test_remote_control_adapter_debug() {
        let adapter = RemoteControlAdapter::new_unchecked(
            RemoteControlConfig::default(),
            crate::dirs::paths(),
        );
        let debug = format!("{:?}", adapter);
        assert!(debug.contains("RemoteControlAdapter"));
    }

    #[test]
    fn test_ssh_cmd_builds() {
        let adapter = RemoteControlAdapter::new_unchecked(
            RemoteControlConfig {
                host: "test.example.com".to_string(),
                user: "admin".to_string(),
                port: 2222,
                protocol: RemoteProtocol::Ssh {
                    key_path: Some("/key".to_string()),
                },
                display: Some(":1".to_string()),
                ssh_extra_args: vec!["-o".to_string(), "Compression=yes".to_string()],
                connect_timeout: Duration::from_secs(5),
            },
            crate::dirs::paths(),
        );

        let cmd = adapter.ssh_cmd();
        // We can't inspect the Command easily, but at least we verified it doesn't
        // panic
        let _ = cmd;
    }
}
