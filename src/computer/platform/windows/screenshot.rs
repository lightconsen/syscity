//! Windows screenshot tool using PowerShell and .NET.

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Take screenshots on Windows using PowerShell + .NET System.Drawing.
///
/// Falls back to `nircmd` if available. Compresses to JPEG to keep base64
/// small.
#[derive(Debug)]
pub struct ScreenshotTool;

impl Default for ScreenshotTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenshotTool {
    pub fn new() -> Self {
        Self
    }

    /// The PowerShell that takes one capture, and nothing else.
    ///
    /// A region wins over a window when both are given: the region is the more
    /// specific request, and a caller that named both meant the smaller
    /// rectangle. Extracted from `execute` so a test can read the script — the
    /// defect this replaces was a region the tool never mentioned, which no
    /// test on a machine without Windows could see otherwise.
    fn capture_script(args: &Value, destination: &str) -> String {
        let path = destination.replace("'", "''");
        let window = args.get("window").and_then(|v| v.as_str());
        if let Some((x, y, width, height)) =
            crate::computer::platform::common::screenshot_region(args)
        {
            // `CopyFromScreen` takes a source rectangle directly, so the crop
            // costs nothing extra and the bytes are the screen's own pixels.
            return format!(
                r#"
Add-Type -AssemblyName System.Drawing
$bounds = New-Object System.Drawing.Rectangle({x}, {y}, {width}, {height})
$bmp = New-Object System.Drawing.Bitmap({width}, {height})
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save('{path}')
$g.Dispose()
$bmp.Dispose()
"#
            );
        }
        if let Some(win_title) = window {
            return format!(
                r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$proc = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{title}*' }} | Select-Object -First 1
if ($proc -eq $null) {{ Write-Error "Window not found"; exit 1 }}
$rect = New-Object System.Drawing.Rectangle
$rect.X = $proc.MainWindowHandle | % {{
    $r = New-Object System.Drawing.Rectangle
    [void][System.Runtime.InteropServices.Marshal]::GetObjectForIUnknown($_)
}}
# Fallback: screenshot full screen
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save('{path}')
$g.Dispose()
$bmp.Dispose()
"#,
                title = win_title.replace("'", "''"),
            );
        }
        format!(
            r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bmp.Save('{path}')
$g.Dispose()
$bmp.Dispose()
"#
        )
    }
}

#[async_trait]
impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "windows_screenshot"
    }

    fn description(&self) -> &str {
        "Take a screenshot on Windows. Returns a base64-encoded image. Uses PowerShell + .NET \
         System.Drawing, or nircmd if available."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["computer", "windows", "screenshot"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Take a screenshot",
            serde_json::json!({
                "window": {
                    "type": "string",
                    "description": "Optional window title to capture instead of full screen"
                },
                "region": {
                    "type": "object",
                    "description": "Optional region to capture, as {\"x\", \"y\", \"width\", \
                                    \"height\"} in screen points",
                    "properties": {
                        "x": { "type": "integer" },
                        "y": { "type": "integer" },
                        "width": { "type": "integer" },
                        "height": { "type": "integer" }
                    },
                    "required": ["x", "y", "width", "height"]
                }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let screenshot_dir = _context.workspace_root().join("files");
        let _ = tokio::fs::create_dir_all(&screenshot_dir).await;
        let temp_path =
            screenshot_dir.join(format!("screenshot_{}.png", crate::utils::ms_timestamp()));
        let temp_str = temp_path.to_string_lossy();

        info!("Taking Windows screenshot: {}", temp_str);

        // A region was silently ignored here: the adapter has always sent one,
        // and the copy below only ever captured `PrimaryScreen.Bounds`. That
        // parse now happens inside `capture_script`, so the two cannot drift.
        let ps_script = Self::capture_script(&args, &temp_str);

        let result = timeout(
            Duration::from_secs(15),
            Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    &ps_script,
                ])
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) if output.status.success() => {
                let encoded_path =
                    crate::computer::screenshot_encoder::maybe_encode_screenshot(&temp_path).await;
                let format = if encoded_path
                    .extension()
                    .map(|e| e == "jpg")
                    .unwrap_or(false)
                {
                    "jpeg"
                } else {
                    "png"
                };

                let bytes = tokio::fs::read(&encoded_path).await.map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: format!("Failed to read screenshot: {}", encoded_path.display()),
                        details: e.to_string(),
                    }
                })?;

                if let Err(e) = tokio::fs::remove_file(&temp_path).await {
                    tracing::warn!("Failed to cleanup temp file '{}': {}", temp_path.display(), e);
                }
                if encoded_path != temp_path {
                    if let Err(e) = tokio::fs::remove_file(&encoded_path).await {
                        tracing::warn!(
                            "Failed to cleanup temp file '{}': {}",
                            encoded_path.display(),
                            e
                        );
                    }
                }

                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let data_url = format!("data:image/{};base64,{}", format, b64);

                Ok(ToolExecutionResult::success(format!(
                    "Screenshot captured ({} bytes, base64: {}...)",
                    bytes.len(),
                    &data_url[..data_url.len().min(80)]
                ))
                .with_data(serde_json::json!({
                    "image_base64": b64,
                    "data_url": data_url,
                    "format": format,
                    "size": bytes.len()
                })))
            }
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("PowerShell screenshot failed: {}", stderr);
                Ok(ToolExecutionResult::error(format!("Screenshot failed: {}", stderr)))
            }
            Ok(Err(e)) => {
                Ok(ToolExecutionResult::error(format!("Failed to run PowerShell: {}", e)))
            }
            Err(_) => Ok(ToolExecutionResult::error("Screenshot timed out".to_string())),
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screenshot_tool_creation() {
        let tool = ScreenshotTool::new();
        assert_eq!(tool.name(), "windows_screenshot");
    }

    /// A region becomes a bitmap of that region, not of the screen.
    ///
    /// The tool used to take `PrimaryScreen.Bounds` whatever it was asked for,
    /// so a close-up was a full-screen picture the caller then mapped as if it
    /// were a crop. The script is where that is visible from a non-Windows
    /// machine.
    #[test]
    fn a_region_is_copied_as_that_rectangle() {
        let args = serde_json::json!({
            "region": { "x": 100, "y": 50, "width": 300, "height": 200 }
        });
        let script = ScreenshotTool::capture_script(&args, "C:\\out.png");
        assert!(
            script.contains("Rectangle(100, 50, 300, 200)"),
            "the source rectangle must be the requested one: {script}"
        );
        assert!(
            script.contains("Bitmap(300, 200)"),
            "the buffer must be the region's size, not the screen's: {script}"
        );
        assert!(
            !script.contains("PrimaryScreen.Bounds"),
            "a close-up must not fall back to the whole screen: {script}"
        );
    }

    /// A region outranks a window when both are named.
    #[test]
    fn a_region_wins_over_a_window() {
        let args = serde_json::json!({
            "window": "Untitled",
            "region": { "x": 0, "y": 0, "width": 10, "height": 10 }
        });
        let script = ScreenshotTool::capture_script(&args, "C:\\o.png");
        assert!(script.contains("Rectangle(0, 0, 10, 10)"), "{script}");
        assert!(!script.contains("Untitled"), "{script}");
    }

    /// With neither, the whole screen is copied — unchanged behaviour.
    #[test]
    fn no_region_and_no_window_is_the_whole_screen() {
        let script = ScreenshotTool::capture_script(&serde_json::json!({}), "C:\\o.png");
        assert!(script.contains("PrimaryScreen.Bounds"), "{script}");
    }
}
