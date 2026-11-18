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
//! // The adapter resolves its workspace staging dir against this root.
//! let paths = Arc::new(syscity::dirs::SyscityPaths::from_env());
//! let adapter = RemoteControlAdapter::new(config, paths).await?;
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
    Rect, Result, Screenshot, Tri, UiElement, WaitCondition,
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

/// The `ssh` options a command line should carry, given where and how to
/// connect.
///
/// Shared with the process runner's SSH backend (`tools::process_runner::remote`)
/// so the two cannot drift into connecting differently: same batch mode, same
/// connect timeout, same host-key policy, same port and key handling. The
/// *destination* is left to the caller, because the adapter always connects as a
/// named user while the runner may leave it to the ssh client's own config.
pub fn ssh_option_args(port: u16, key_path: Option<&str>, extra_args: &[String]) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-p".to_string(),
        port.to_string(),
    ];

    if let Some(key) = key_path {
        args.push("-i".to_string());
        args.push(key.to_string());
    }

    args.extend(extra_args.iter().cloned());
    args
}

/// How to name the remote display so the command on the *other* side sees it.
///
/// `Command::env` sets the local `ssh` client's environment, which reaches the
/// remote shell only if the server's `sshd_config` has the name in
/// `AcceptEnv` — Debian and Ubuntu ship `AcceptEnv LANG LC_*`, so `DISPLAY` is
/// silently dropped and an X tool would talk to whatever display the server
/// chose, or to none. Prefixing the remote command is unconditional: the
/// remote shell sets `DISPLAY` itself, and no server configuration is involved.
///
/// Quoted because `display` comes from configuration and is interpolated into
/// a shell string; a value containing a space or `;` must not become a second
/// command.
fn remote_display_prefix(display: Option<&str>) -> Option<String> {
    let display = display?.trim();
    if display.is_empty() {
        return None;
    }
    Some(format!("DISPLAY={}", shell_word(display)))
}

/// Wrap a remote command line so the display is set on the far side.
#[allow(dead_code)] // kept as the readable form; `run_remote` builds its line directly
fn with_remote_display(command: &str, display: Option<&str>) -> String {
    match remote_display_prefix(display) {
        Some(prefix) => format!("{prefix} {command}"),
        None => command.to_string(),
    }
}

/// Quote one word for the remote shell.
///
/// ssh joins its arguments into a single string the remote shell parses, so an
/// argument with a space (a PowerShell script, a path with spaces) would be
/// split into several. Single quotes keep everything literal; an embedded
/// quote is closed, escaped and reopened (`'` → `'\''`).
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%^".contains(c))
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The remote `screencapture` argument list, with the region in it.
///
/// `screencapture -R x,y,w,h` takes logical screen coordinates and grabs that
/// rectangle; without it the command captures every display. The two are not
/// interchangeable: a caller that asked for a region and got the whole screen
/// gets a picture of the wrong thing, and cannot tell from the result.
fn macos_screenshot_args(region: Option<Rect>) -> Vec<String> {
    let mut args: Vec<String> = ["-x", "-t", "png"].iter().map(|s| s.to_string()).collect();
    if let Some(r) = region {
        args.push("-R".to_string());
        args.push(format!("{},{},{},{}", r.x, r.y, r.width, r.height));
    }
    // The trailing `-` is "write to stdout", which the SSH pipe reads.
    args.push("-".to_string());
    args
}

/// The PowerShell that captures the remote Windows screen, cropped to `region`.
///
/// `CopyFromScreen(sourceOrigin, destinationOrigin, blockRegionSize)` — the
/// first argument is what makes a region possible. The uncropped form captured
/// `PrimaryScreen.Bounds` from `Empty`, which is the same call with a fixed
/// origin, so passing the region costs nothing but the two coordinates.
///
/// The origin is the region's own top-left, and the block is its size, so the
/// bitmap is the requested rectangle and not the screen it was cut from.
///
/// `SetProcessDPIAware` is not decoration: the coordinates a region carries
/// come from the UI Automation tree, which reports **physical** pixels, while a
/// DPI-unaware PowerShell has `CopyFromScreen` and `PrimaryScreen.Bounds`
/// virtualised for the session's scale factor. Without it, a region capture on
/// a scaled display crops the wrong pixels and a full-screen capture returns a
/// scaled-down bitmap.
fn windows_screenshot_script(region: Option<Rect>) -> String {
    let (origin, size) = match region {
        Some(r) => (
            format!("New-Object System.Drawing.Point({}, {})", r.x, r.y),
            format!("New-Object System.Drawing.Size({}, {})", r.width, r.height),
        ),
        None => ("[System.Drawing.Point]::Empty".to_string(), "$screen.Size".to_string()),
    };
    // A full-screen capture keeps its existing shape: the screen's own bounds
    // are the block. A region supplies both the origin and the size.
    let bounds = if region.is_some() {
        String::new()
    } else {
        "$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds\n".to_string()
    };
    format!(
        r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class SCDpi {{
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}}
'@
[SCDpi]::SetProcessDPIAware() | Out-Null
{bounds}$origin = {origin}
$size = {size}
$bitmap = New-Object System.Drawing.Bitmap($size.Width, $size.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($origin, [System.Drawing.Point]::Empty, $size)
$ms = New-Object System.IO.MemoryStream
$bitmap.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
[System.Convert]::ToBase64String($ms.ToArray())
"#
    )
}

/// The remote Linux screenshot arguments for one tool, with the region in it.
///
/// The three tools are not interchangeable, and only one of them can be told a
/// rectangle on the command line:
///
/// - `scrot -a X,Y,W,H FILE` — explicitly "non-interactively choose a
///   rectangle" (scrot(1)), with `FILE` of `-` meaning stdout;
/// - `gnome-screenshot -a` — the `-a` flag opens the *interactive* area
///   selector. It takes no rectangle, so over SSH it waits for a mouse drag
///   that never comes and the whole capture hangs. Its arguments therefore
///   never carry the region, and [`screenshot_linux`] will not try it when one
///   was asked for;
/// - ImageMagick's `import` — selects the *window under the pointer* and waits
///   for a click unless told otherwise, so it needs `-window root` to capture
///   non-interactively. The rectangle then comes from `-crop WxH+X+Y`; the
///   output file is the last argument.
fn linux_screenshot_args(tool: &str, region: Option<Rect>) -> Vec<String> {
    match tool {
        "scrot" => {
            let mut args = Vec::new();
            if let Some(r) = region {
                args.push("-a".to_string());
                args.push(format!("{},{},{},{}", r.x, r.y, r.width, r.height));
            }
            args.push("-".to_string());
            args
        }
        "gnome-screenshot" => vec!["-f".to_string(), "/dev/stdout".to_string()],
        _ => {
            let mut args = vec!["-window".to_string(), "root".to_string()];
            if let Some(r) = region {
                args.push("-crop".to_string());
                args.push(format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y));
            }
            args.push("png:-".to_string());
            args
        }
    }
}

/// Whether this tool can be told an exact rectangle without asking a person.
///
/// `gnome-screenshot -a` only opens a drag-select box, so a region capture must
/// skip it rather than hang the SSH session waiting for a mouse.
fn linux_tool_takes_a_region(tool: &str) -> bool {
    tool != "gnome-screenshot"
}

/// Whether a decoded capture holds no content at all.
///
/// "Blank" is one flat colour across the whole frame — the shape a machine
/// looks like when the X server or the desktop session is up but nobody is
/// logged in. It is deliberately **not** "mostly dark": a video player, a dark
/// theme, or a full-screen image can be nearly black and perfectly real, and
/// refusing those would make this check a bug of its own. Only a single
/// uniform colour, which no usable desktop ever is, counts.
///
/// Every pixel is read, not a sample. The alternative was measured first and
/// is unsafe: a coarse grid misses thin content, so a real screen with one
/// bright cursor-line would be reported blank and refused — a false refusal is
/// worse than the cost of one pass over a decoded frame that was just decoded
/// anyway.
#[cfg(feature = "image")]
fn is_blank_frame(img: &image::DynamicImage) -> bool {
    let img = img.to_rgb8();
    let (width, height) = img.dimensions();
    if width == 0 || height == 0 {
        return true;
    }

    let first = img.get_pixel(0, 0);
    img.pixels().all(|pixel| pixel == first)
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

        // `RemoteProtocol` has one variant, so this destructure cannot fail.
        let RemoteProtocol::Ssh { key_path } = &self.config.protocol;
        for arg in
            ssh_option_args(self.config.port, key_path.as_deref(), &self.config.ssh_extra_args)
        {
            cmd.arg(arg);
        }

        cmd.arg(format!("{}@{}", self.config.user, self.config.host));
        cmd
    }

    /// Run a command on the remote host via SSH.
    ///
    /// The display is set with a prefix on the *remote* command, not with
    /// `Command::env`: the latter sets this process's environment for the ssh
    /// client, which forwards nothing unless the server's `AcceptEnv` names the
    /// variable, and Debian/Ubuntu name only `LANG LC_*`. See
    /// [`remote_display_prefix`].
    ///
    /// The whole call is bounded by `connect_timeout`, because the one failure
    /// this adapter cannot otherwise survive is a remote tool that *waits* — a
    /// selector dialog, a missing auth cookie prompting for a password — and
    /// never returns. `ConnectTimeout` only bounds establishing the connection.
    async fn run_remote(&self, cmd: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut ssh = self.ssh_cmd();
        let mut line = String::new();
        if let Some(prefix) = remote_display_prefix(self.config.display.as_deref()) {
            line.push_str(&prefix);
            line.push(' ');
        }
        line.push_str(&shell_word(cmd));
        for arg in args {
            line.push(' ');
            line.push_str(&shell_word(arg));
        }
        ssh.arg(&line);

        let timeout = self.config.connect_timeout;
        match tokio::time::timeout(timeout, ssh.output()).await {
            Ok(output) => output,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("remote command `{cmd}` did not finish within {timeout:?}"),
            )),
        }
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
        // Try gnome-screenshot first, then scrot, then import. Only scrot and
        // import can be told a rectangle; `gnome-screenshot -a` opens a
        // drag-select and would wait forever for a mouse that SSH cannot
        // provide, so a region capture skips it — see [`linux_screenshot_args`]
        // and [`linux_tool_takes_a_region`].
        const TOOLS: [&str; 3] = ["gnome-screenshot", "scrot", "import"];

        for tool in TOOLS {
            if region.is_some() && !linux_tool_takes_a_region(tool) {
                continue;
            }

            let tool_args = linux_screenshot_args(tool, region);
            let output = self
                .run_remote(tool, &tool_args.iter().map(String::as_str).collect::<Vec<_>>())
                .await
                .map_err(|e| {
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

                #[cfg(feature = "image")]
                let (width, height, blank) = if let Ok(img) = image::load_from_memory(&final_bytes)
                {
                    let blank = is_blank_frame(&img);
                    (img.width(), img.height(), blank)
                } else {
                    (0, 0, false)
                };
                #[cfg(not(feature = "image"))]
                let (width, height, blank) = (0, 0, false);

                if blank {
                    return Err(ComputerError::BlankScreen(format!(
                        "{} on {} returned a single flat colour ({}x{}); the display is up but \
                         no desktop session is drawing on it",
                        tool, self.config.host, width, height
                    )));
                }

                let base64 = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &final_bytes,
                );
                return Ok(Screenshot::new(base64, width, height));
            }
        }

        Err(ComputerError::ScreenshotFailed(format!(
            "All remote screenshot tools failed on Linux host {} (tried {}, and the region was \
             requested: {})",
            self.config.host,
            TOOLS.join(", "),
            region.is_some()
        )))
    }

    async fn screenshot_macos(&self, region: Option<Rect>) -> Result<Screenshot> {
        let args = macos_screenshot_args(region);
        let output = self
            .run_remote("screencapture", &args.iter().map(String::as_str).collect::<Vec<_>>())
            .await
            .map_err(|e| {
                ComputerError::ScreenshotFailed(format!(
                    "screencapture failed on remote macOS: {}",
                    e
                ))
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
        let (width, height) = {
            match image::load_from_memory(&final_bytes) {
                Ok(img) => {
                    if is_blank_frame(&img) {
                        return Err(ComputerError::BlankScreen(format!(
                            "screencapture on {} returned a single flat colour ({}x{}); the \
                             display is up but no desktop session is drawing on it (locked, or \
                             nobody logged in)",
                            self.config.host,
                            img.width(),
                            img.height()
                        )));
                    }
                    (img.width(), img.height())
                }
                Err(_) => (0, 0),
            }
        };
        #[cfg(not(feature = "image"))]
        let (width, height) = (0, 0);

        Ok(Screenshot::new(base64, width, height))
    }

    async fn screenshot_windows(&self, region: Option<Rect>) -> Result<Screenshot> {
        let ps = windows_screenshot_script(region);

        let output = self
            .run_remote("powershell", &["-Command", &ps])
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
                // Move and click in one script. `Cursor::Position` alone only
                // moves the pointer: reporting "clicked" after it would claim
                // an action that never happened. `mouse_event` with the
                // down/up pair for the requested button is the Win32 way to
                // deliver a click at the current position.
                let (down, up) = match button {
                    MouseButton::Left => ("0x0002", "0x0004"),
                    MouseButton::Middle => ("0x0020", "0x0040"),
                    MouseButton::Right => ("0x0008", "0x0010"),
                };
                let ps = format!(
                    r#"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({x}, {y}); Add-Type @'
using System;
using System.Runtime.InteropServices;
public class SCMouse {{
    [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, UIntPtr dwExtraInfo);
}}
'@; [SCMouse]::mouse_event({down}, 0, 0, 0, [UIntPtr]::Zero); [SCMouse]::mouse_event({up}, 0, 0, 0, [UIntPtr]::Zero)"#,
                    x = point.x,
                    y = point.y,
                );
                self.run_remote_text("powershell", &["-Command", &ps])
                    .await?;
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

    /// Move the pointer on the remote host, pressing nothing.
    ///
    /// A hover and a bare move are the same gesture to every remote OS, so
    /// they share one path. macOS uses `cliclick`'s bare `m:` (the click
    /// spellings all press); Linux uses `xdotool mousemove`; Windows sets
    /// `Cursor::Position` with no `mouse_event` after it.
    async fn move_remote(&self, point: Point) -> Result<()> {
        match self.remote_os {
            RemoteOs::Linux => {
                self.run_remote_text(
                    "xdotool",
                    &["mousemove", &point.x.to_string(), &point.y.to_string()],
                )
                .await?;
            }
            RemoteOs::Macos => {
                self.run_remote_text("cliclick", &[&format!("m:{},{}", point.x, point.y)])
                    .await?;
            }
            RemoteOs::Windows => {
                let ps = format!(
                    r#"Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({}, {})"#,
                    point.x, point.y
                );
                self.run_remote_text("powershell", &["-Command", &ps])
                    .await?;
            }
            _ => {
                return Err(ComputerError::UnsupportedPlatform(format!(
                    "Remote pointer move not supported for OS {:?}",
                    self.remote_os
                )))
            }
        }
        Ok(())
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
        // Each injector spells named keys its own way; a misspelt one types
        // the word into the focused field instead of pressing the key.
        let joined = match self.remote_os {
            RemoteOs::Linux => keys
                .iter()
                .map(|k| crate::computer::evdev_key_name(k))
                .collect::<Vec<_>>()
                .join("+"),
            RemoteOs::Macos => keys
                .iter()
                .map(|k| match crate::computer::canonical_key(k) {
                    "enter" => "return".to_string(),
                    "escape" => "esc".to_string(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("+"),
            RemoteOs::Windows => keys
                .iter()
                .map(|k| match crate::computer::canonical_key(k) {
                    "enter" => "~".to_string(),
                    "space" => " ".to_string(),
                    other => other.to_string(),
                })
                .collect::<String>(),
            _ => keys.join("+"),
        };
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
                                bounds: Some(Rect::new(
                                    coords[0],
                                    coords[1],
                                    coords[2] as u32,
                                    coords[3] as u32,
                                )),
                                enabled: Tri::Known(meta[2].trim() == "1"),
                                focused: Tri::Unknown,
                                actions: None,
                                children: vec![],
                                children_count: None,
                                truncated: false,
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
                        bounds: None,
                        enabled: Tri::Unknown,
                        focused: Tri::Unknown,
                        actions: None,
                        children: vec![],
                        children_count: None,
                        truncated: false,
                    });
                }
            }
            if !elements.is_empty() {
                return Ok(elements);
            }
        }

        warn!("No accessible UI tree tools found on remote Linux (try installing python3-pyatspi)");
        Err(ComputerError::UnsupportedPlatform(format!(
            "cannot read the accessibility tree on remote Linux host {}: `python3` with pyatspi \
             (install python3-pyatspi) and `wmctrl` both returned nothing. A missing tree is not \
             an empty screen — actions that name an element cannot be aimed without one",
            self.config.host
        )))
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
                                bounds: Some(Rect::new(
                                    coords[0],
                                    coords[1],
                                    coords[2] as u32,
                                    coords[3] as u32,
                                )),
                                enabled: Tri::Known(meta[2].trim() == "True"),
                                focused: Tri::Unknown,
                                actions: None,
                                children: vec![],
                                children_count: None,
                                truncated: false,
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
        Err(ComputerError::UnsupportedPlatform(format!(
            "cannot read the accessibility tree on remote Windows host {}: the UIAutomation \
             script returned no elements. Either .NET UIAutomation is unavailable, or the SSH \
             session is in Windows' non-interactive session 0, where it sees no user desktop",
            self.config.host
        )))
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

    async fn display_geometry(&self) -> Result<crate::computer::display::DisplayGeometry> {
        // The displays belong to the remote host; probing this machine would
        // describe the wrong screen. Until the remote side reports its layout,
        // the honest answer is "unknown" and callers must not convert.
        Ok(crate::computer::display::DisplayGeometry::default())
    }

    async fn read_ui_tree(&self, app: Option<&str>) -> Result<Vec<UiElement>> {
        match self.remote_os {
            RemoteOs::Linux => self.read_ui_tree_linux(app).await,
            RemoteOs::Windows => self.read_ui_tree_windows(app).await,
            RemoteOs::Macos => Err(ComputerError::UnsupportedPlatform(format!(
                "cannot read the accessibility tree on remote macOS host {}: AXUIElement is not \
                 reachable over SSH. Usable only through the screenshot/vision path, and a \
                 caller that needs element ids has none here",
                self.config.host
            ))),
            RemoteOs::Unknown => Err(ComputerError::UnsupportedPlatform(format!(
                "cannot read the accessibility tree on remote host {}: its OS could not be \
                 identified during connection",
                self.config.host
            ))),
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
            DesktopAction::Type { text, .. } => self.type_remote(&text).await,
            DesktopAction::KeyPress { keys } => self.keypress_remote(&keys).await,
            DesktopAction::Hover { target } => {
                let point = match target {
                    ClickTarget::Coordinate(p) => p,
                    _ => {
                        return Err(ComputerError::ElementNotFound(
                            "Remote adapter only supports coordinate hovers".to_string(),
                        ));
                    }
                };
                self.move_remote(point).await?;
                Ok(ActionResult::success(
                    "the pointer is now over the target; the next capture shows whether the \
                     control reacted",
                )
                .with_data(serde_json::json!({ "pointer": { "x": point.x, "y": point.y } })))
            }
            DesktopAction::PointerMove { to } => {
                self.move_remote(to).await?;
                Ok(ActionResult::success(format!(
                    "the pointer moved to ({}, {}); nothing was pressed",
                    to.x, to.y
                )))
            }
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

    // ── Region handling ────────────────────────────────────────────────────
    //
    // A screenshot is the one thing a "remote help" feature is for, and a
    // region that is silently dropped returns a picture of the whole screen
    // that looks like a success. Each platform names its rectangle
    // differently, so each gets its own assertion rather than one shared
    // "contains the numbers" check.

    const REGION: Rect = Rect {
        x: 120,
        y: 60,
        width: 640,
        height: 480,
    };

    #[test]
    fn macos_screenshot_region_uses_screencapture_r() {
        let argv = macos_screenshot_args(Some(REGION));
        let args = argv.join(" ");
        assert!(
            args.contains("-R 120,60,640,480"),
            "screencapture takes the rectangle after -R: {args}"
        );
        // Still writing to the pipe, and still silent. Checked as tokens: `-x`
        // is the first flag, so a padded substring match would never see it.
        assert!(argv.iter().any(|a| a == "-x"), "must not play a sound: {args}");
        assert!(
            argv.windows(2).any(|w| w[0] == "-t" && w[1] == "png"),
            "must ask for PNG: {args}"
        );
        assert_eq!(argv.last().map(String::as_str), Some("-"), "must write to stdout: {args}");
    }

    #[test]
    fn macos_screenshot_without_a_region_captures_everything() {
        let args = macos_screenshot_args(None).join(" ");
        assert!(!args.contains("-R"), "no region means every display, not a crop of one: {args}");
    }

    #[test]
    fn windows_screenshot_region_moves_the_copy_origin() {
        let script = windows_screenshot_script(Some(REGION));
        assert!(
            script.contains("New-Object System.Drawing.Point(120, 60)"),
            "the region's top-left is the copy origin: {script}"
        );
        assert!(
            script.contains("New-Object System.Drawing.Size(640, 480)"),
            "the block is the region's size: {script}"
        );
        // A region is a small bitmap, so the full screen must not be measured
        // and used instead — that is how the region got dropped before.
        assert!(
            !script.contains("$screen"),
            "a region must not fall back to the primary screen's bounds: {script}"
        );
        // The origin must be a variable, not `::Empty`, or the crop starts at
        // the top-left of the screen whichever rectangle was asked for.
        assert!(
            !script.contains("CopyFromScreen([System.Drawing.Point]::Empty"),
            "the source origin must not be pinned to Empty: {script}"
        );
    }

    #[test]
    fn windows_screenshot_without_a_region_keeps_the_whole_screen() {
        let script = windows_screenshot_script(None);
        assert!(script.contains("PrimaryScreen.Bounds"));
        assert!(script.contains("[System.Drawing.Point]::Empty"));
        assert!(script.contains("$screen.Size"));
    }

    /// A region's numbers come from the UI Automation tree, which is in physical
    /// pixels; a DPI-unaware process sees a virtualised screen. Without the
    /// call, the crop offsets by the scale factor on any scaled display.
    #[test]
    fn windows_screenshot_is_dpi_aware() {
        for region in [Some(REGION), None] {
            let script = windows_screenshot_script(region);
            assert!(
                script.contains("[SCDpi]::SetProcessDPIAware()"),
                "the capture must not be virtualised by the display scale: {script}"
            );
        }
    }

    /// The two Linux tools that can be told a rectangle get it, each in its own
    /// syntax. The bug this closes: only `import` was given the rectangle, and
    /// it is the third choice, so a host with `scrot` answered with the whole
    /// screen.
    #[test]
    fn the_linux_tools_that_can_take_a_region_are_told_it() {
        // scrot's `-a` is documented as "non-interactively choose a rectangle".
        let args = linux_screenshot_args("scrot", Some(REGION)).join(" ");
        assert!(args.contains("-a 120,60,640,480"), "scrot: {args}");
        assert!(args.ends_with(" -"), "scrot reads its file from the last argument: {args}");

        // import names the rectangle with `-crop WxH+X+Y`, and must say
        // `-window root` or it waits for a click on the window under the
        // pointer — which over SSH is a hang, not a capture.
        let args = linux_screenshot_args("import", Some(REGION)).join(" ");
        assert!(args.contains("-crop 640x480+120+60"), "import: {args}");
        assert!(args.contains("-window root"), "import must not wait for a click: {args}");
        assert!(args.ends_with(" png:-"), "import writes to stdout: {args}");
    }

    /// `gnome-screenshot -a` is the interactive drag-select, so a region must
    /// skip it. Handing it `-a` plus a rectangle would send a flag it cannot
    /// parse, and handing it bare `-a` would hang the SSH session on a mouse
    /// drag that no one is there to perform.
    #[test]
    fn gnome_screenshot_is_skipped_when_a_region_is_asked_for() {
        assert!(!linux_tool_takes_a_region("gnome-screenshot"));
        assert!(linux_tool_takes_a_region("scrot"));
        assert!(linux_tool_takes_a_region("import"));

        // Its argument list never carries the region, so it cannot be the
        // accidental answer to a region request.
        let args = linux_screenshot_args("gnome-screenshot", Some(REGION)).join(" ");
        assert!(!args.contains("-a"), "no interactive selector: {args}");
        assert!(!args.contains("120,60,640,480"), "it takes no rectangle: {args}");
    }

    /// Each tool still does its own job when no region was asked for: this is
    /// the call the common case makes, and a stray crop would cut it.
    #[test]
    fn linux_screenshot_without_a_region_asks_for_none() {
        for tool in ["gnome-screenshot", "scrot", "import"] {
            let args = linux_screenshot_args(tool, None).join(" ");
            assert!(!args.contains("-a "), "{tool} must not crop: {args}");
            assert!(!args.contains("-crop"), "{tool} must not crop: {args}");
        }
        // Every tool still names a destination.
        assert!(linux_screenshot_args("gnome-screenshot", None)
            .join(" ")
            .contains("/dev/stdout"));
        assert_eq!(linux_screenshot_args("scrot", None), vec!["-".to_string()]);
        assert_eq!(
            linux_screenshot_args("import", None),
            vec![
                "-window".to_string(),
                "root".to_string(),
                "png:-".to_string()
            ]
        );
    }

    // ── The display must be set on the far side ────────────────────────────

    /// `Command::env("DISPLAY", …)` only ever reached the local ssh client.
    /// A server's `AcceptEnv` decides whether it crosses, and Debian/Ubuntu
    /// allow `LANG LC_*` and nothing else — so the remote command ran with no
    /// display at all, and every X tool failed for a reason the caller could
    /// not see. The prefix is what actually sets it.
    #[test]
    fn the_display_is_set_on_the_remote_command() {
        assert_eq!(remote_display_prefix(Some(":0")).as_deref(), Some("DISPLAY=:0"));
        assert_eq!(remote_display_prefix(Some(" :1 ")).as_deref(), Some("DISPLAY=:1"));
        assert_eq!(remote_display_prefix(None), None);
        assert_eq!(remote_display_prefix(Some("   ")), None);

        assert_eq!(with_remote_display("scrot -", Some(":0")), "DISPLAY=:0 scrot -");
        assert_eq!(with_remote_display("scrot -", None), "scrot -");
    }

    /// The display name comes from configuration and is interpolated into a
    /// shell line, so it must not be able to become a second command.
    #[test]
    fn a_display_with_shell_syntax_is_quoted_not_executed() {
        let prefix = remote_display_prefix(Some(":0; rm -rf /")).expect("a prefix");
        assert!(
            prefix.starts_with("DISPLAY='") && prefix.ends_with('\''),
            "the value must be one quoted word: {prefix}"
        );
        assert_eq!(prefix, "DISPLAY=':0; rm -rf /'");
    }

    /// ssh joins its arguments into one string the remote shell parses, so an
    /// argument with a space would silently split into several.
    #[test]
    fn an_argument_with_a_space_stays_one_word() {
        assert_eq!(shell_word("xdotool"), "xdotool");
        assert_eq!(shell_word("640x480+120+60"), "640x480+120+60");
        assert_eq!(shell_word("/dev/stdout"), "/dev/stdout");
        assert_eq!(shell_word("png:-"), "png:-");

        let script = "Add-Type -AssemblyName System.Windows.Forms; 'x'";
        let quoted = shell_word(script);
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        // The embedded quote is closed, escaped and reopened.
        assert!(quoted.contains(r"'\''"), "a quote must be escaped, not ended: {quoted}");
        assert!(
            !quoted[1..quoted.len() - 1].contains("'; "),
            "must not leave the shell: {quoted}"
        );
    }

    // ── A blank screen is not a success ────────────────────────────────────

    /// A machine that is up with no session logged in answers every capture
    /// with one flat colour. Returning it as a screenshot is how a run decides
    /// against a screen that holds nothing and then clicks into it.
    #[cfg(feature = "image")]
    #[test]
    fn a_flat_frame_is_blank_and_any_content_is_not() {
        let flat = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            48,
            image::Rgb([0, 0, 0]),
        ));
        assert!(is_blank_frame(&flat), "one colour is blank");

        // Not "dark": a real screen can be nearly black all over.
        let dark_but_real = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            48,
            image::Rgb([3, 3, 4]),
        ));
        assert!(is_blank_frame(&dark_but_real), "uniform is uniform, dark or not");

        // One pixel of content is content. A video player, a dark theme, a
        // full-screen photo — all of these have something that is not the
        // background.
        let mut with_content = image::RgbImage::from_pixel(64, 48, image::Rgb([0, 0, 0]));
        with_content.put_pixel(31, 24, image::Rgb([255, 255, 255]));
        assert!(
            !is_blank_frame(&image::DynamicImage::ImageRgb8(with_content)),
            "a screen with anything on it is not blank"
        );

        // A zero-sized frame describes no screen at all.
        let empty = image::DynamicImage::ImageRgb8(image::RgbImage::new(0, 0));
        assert!(is_blank_frame(&empty));
    }
}
