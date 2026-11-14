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

        let window = args.get("window").and_then(|v| v.as_str());

        let ps_script = if let Some(win_title) = window {
            format!(
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
                path = temp_str.replace("'", "''")
            )
        } else {
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
"#,
                path = temp_str.replace("'", "''")
            )
        };

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
}
