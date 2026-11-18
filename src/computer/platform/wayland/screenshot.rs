//! Linux Wayland screenshot tool using external capture utilities.

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Take screenshots on Linux Wayland using available capture tools.
///
/// Tries tools in order: `grim` → `spectacle` → `gnome-screenshot`.
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
        for cmd in &["grim", "spectacle", "gnome-screenshot"] {
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

    /// `grim`'s argv. `-g` takes `x,y WxH` — the origin and size with a space.
    ///
    /// Takes the raw tool arguments so a test runs the same parse-then-build
    /// path `execute` runs; testing the formatter alone would pass while the
    /// caller ignored the region.
    fn grim_arguments(args: &Value, destination: &std::path::Path) -> Vec<String> {
        let mut argv = Vec::new();
        if let Some(region) = crate::computer::platform::common::screenshot_region_grim(args) {
            argv.push("-g".to_string());
            argv.push(region);
        }
        argv.push(destination.display().to_string());
        argv
    }
}

#[async_trait]
impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "linux_wayland_screenshot"
    }

    fn description(&self) -> &str {
        "Take a screenshot on Linux Wayland. Returns a base64-encoded PNG image. Tries grim, \
         spectacle, or gnome-screenshot."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["computer", "wayland", "screenshot"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Take a screenshot",
            serde_json::json!({
                "region": {
                    "type": "string",
                    "description": "Optional region to capture as x,y w h (e.g. \"100,100 500x300\") for grim"
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
                    "No screenshot tool found. Install grim, spectacle, or gnome-screenshot."
                        .to_string(),
                ));
            }
        };

        let screenshot_dir = _context.workspace_root().join("files");
        let _ = tokio::fs::create_dir_all(&screenshot_dir).await;
        let temp_path =
            screenshot_dir.join(format!("screenshot_{}.png", crate::utils::ms_timestamp()));

        info!("Taking Wayland screenshot with {}: {}", tool, temp_path.display());

        let result = match tool {
            "grim" => {
                let mut cmd = Command::new("grim");
                for arg in Self::grim_arguments(&args, &temp_path) {
                    cmd.arg(arg);
                }
                cmd.output().await
            }
            "spectacle" => {
                let mut cmd = Command::new("spectacle");
                cmd.arg("-f").arg("-n").arg("-o").arg(&temp_path);
                cmd.output().await
            }
            "gnome-screenshot" => {
                let mut cmd = Command::new("gnome-screenshot");
                cmd.arg("-f").arg(&temp_path);
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
        std::env::var("WAYLAND_DISPLAY").is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screenshot_tool_creation() {
        let tool = ScreenshotTool::new();
        assert_eq!(tool.name(), "linux_wayland_screenshot");
    }

    /// The adapter's region reaches `grim` as `x,y WxH`, which is its grammar.
    ///
    /// `grim -g` is not comma-separated: it takes the origin and the size with
    /// a space between them. The comma form would either be rejected or, worse,
    /// parsed as a rectangle nobody chose.
    #[test]
    fn grim_is_given_the_region_in_its_own_grammar() {
        let args = serde_json::json!({
            "region": { "x": 100, "y": 50, "width": 300, "height": 200 }
        });
        assert_eq!(
            ScreenshotTool::grim_arguments(&args, std::path::Path::new("/tmp/out.png")),
            vec!["-g", "100,50 300x200", "/tmp/out.png"]
        );
    }

    /// With no region `grim` captures the output, not a zero-size crop.
    #[test]
    fn a_whole_screen_capture_passes_no_geometry() {
        let argv = ScreenshotTool::grim_arguments(
            &serde_json::json!({}),
            std::path::Path::new("/tmp/out.png"),
        );
        assert!(!argv.iter().any(|a| a == "-g"), "{argv:?}");
    }
}
