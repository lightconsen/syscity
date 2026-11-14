//! Linux X11 screenshot tool using external capture utilities.

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Take screenshots on Linux X11 using available capture tools.
///
/// Tries tools in order: `maim` → `import` (ImageMagick) → `gnome-screenshot` →
/// `flameshot`.
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

    /// Find the first available screenshot utility.
    async fn find_capture_tool() -> Option<&'static str> {
        for cmd in &["maim", "import", "gnome-screenshot", "flameshot"] {
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
}

#[async_trait]
impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "linux_x11_screenshot"
    }

    fn description(&self) -> &str {
        "Take a screenshot on Linux X11. Returns a base64-encoded PNG image. Tries maim, import \
         (ImageMagick), gnome-screenshot, or flameshot."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["computer", "x11", "screenshot"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Take a screenshot",
            serde_json::json!({
                "display": {
                    "type": "integer",
                    "description": "Display number to capture (default: :0)",
                    "default": 0
                },
                "window": {
                    "type": "string",
                    "description": "Optional window name or ID to capture instead of full screen"
                },
                "region": {
                    "type": "string",
                    "description": "Optional region to capture as x,y,w,h (e.g. \"100,100,500,300\")"
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
        let tool = match Self::find_capture_tool().await {
            Some(t) => t,
            None => {
                return Ok(ToolExecutionResult::error(
                    "No screenshot tool found. Install maim, imagemagick, gnome-screenshot, or \
                     flameshot."
                        .to_string(),
                ));
            }
        };

        let screenshot_dir = _context.workspace_root().join("files");
        let _ = tokio::fs::create_dir_all(&screenshot_dir).await;
        let temp_path =
            screenshot_dir.join(format!("screenshot_{}.png", crate::utils::ms_timestamp()));

        info!("Taking X11 screenshot with {}: {}", tool, temp_path.display());

        let region = args.get("region").and_then(|v| v.as_str());
        let window = args.get("window").and_then(|v| v.as_str());

        let result = match tool {
            "maim" => {
                let mut cmd = Command::new("maim");
                if let Some(reg) = region {
                    cmd.arg("-g").arg(reg);
                }
                if let Some(win) = window {
                    cmd.arg("-i").arg(win);
                }
                cmd.arg(&temp_path).output().await
            }
            "import" => {
                let mut cmd = Command::new("import");
                cmd.arg("-window").arg(window.unwrap_or("root"));
                if let Some(reg) = region {
                    cmd.arg("-crop").arg(reg);
                }
                cmd.arg(&temp_path).output().await
            }
            "gnome-screenshot" => {
                let mut cmd = Command::new("gnome-screenshot");
                cmd.arg("-f").arg(&temp_path);
                if window.is_some() {
                    cmd.arg("-w");
                }
                cmd.output().await
            }
            "flameshot" => {
                let mut cmd = Command::new("flameshot");
                cmd.arg("full").arg("-p").arg(&temp_path);
                cmd.output().await
            }
            _ => unreachable!(),
        };

        let output = match timeout(Duration::from_secs(15), async { result }).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Ok(ToolExecutionResult::error(format!("Failed to run {}: {}", tool, e)));
            }
            Err(_) => {
                return Ok(ToolExecutionResult::error(format!("{} timed out", tool)));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("{} failed: {}", tool, stderr);
            return Ok(ToolExecutionResult::error(format!("{} failed: {}", tool, stderr)));
        }

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
                tracing::warn!("Failed to cleanup temp file '{}': {}", encoded_path.display(), e);
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

    fn is_available(&self, _context: &ToolContext) -> bool {
        std::env::var("DISPLAY").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screenshot_tool_creation() {
        let tool = ScreenshotTool::new();
        assert_eq!(tool.name(), "linux_x11_screenshot");
    }
}
