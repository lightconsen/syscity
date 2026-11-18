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

    /// `maim`'s argv. `-g` takes `x,y,w,h`.
    ///
    /// Takes the raw tool arguments rather than a parsed region, so a test
    /// exercises the same parse-then-build path `execute` runs. A builder
    /// tested on its own would have passed while the caller dropped the region.
    fn maim_arguments(args: &Value, destination: &std::path::Path) -> Vec<String> {
        let region = crate::computer::platform::common::screenshot_region_commas(args);
        let window = args.get("window").and_then(|v| v.as_str());
        let mut argv = Vec::new();
        if let Some(region) = region {
            argv.push("-g".to_string());
            argv.push(region);
        }
        if let Some(window) = window {
            argv.push("-i".to_string());
            argv.push(window.to_string());
        }
        argv.push(destination.display().to_string());
        argv
    }

    /// ImageMagick's `import` argv. `-crop` takes `wxh+x+y`, not `x,y,w,h`.
    ///
    /// The order is the point: a rectangle written in the adapter's order is
    /// silently a crop of a different shape (width and height swapped for x
    /// and y), and the picture that comes back looks plausible.
    fn import_arguments(args: &Value, destination: &std::path::Path) -> Vec<String> {
        let region = crate::computer::platform::common::screenshot_region(args);
        let window = args.get("window").and_then(|v| v.as_str());
        let mut argv = vec!["-window".to_string(), window.unwrap_or("root").to_string()];
        if let Some((x, y, width, height)) = region {
            argv.push("-crop".to_string());
            argv.push(format!("{width}x{height}+{x}+{y}"));
        }
        argv.push(destination.display().to_string());
        argv
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

        let window = args.get("window").and_then(|v| v.as_str());

        let result = match tool {
            "maim" => {
                let mut cmd = Command::new("maim");
                for arg in Self::maim_arguments(&args, &temp_path) {
                    cmd.arg(arg);
                }
                cmd.output().await
            }
            "import" => {
                let mut cmd = Command::new("import");
                for arg in Self::import_arguments(&args, &temp_path) {
                    cmd.arg(arg);
                }
                cmd.output().await
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

    /// The adapter's region reaches `maim`'s command line.
    ///
    /// Asserted through the argv builder rather than the parser: reading the
    /// region and then not passing it on is the failure this replaces.
    #[test]
    fn maim_is_given_the_region_at_full_resolution() {
        let args = serde_json::json!({
            "region": { "x": 100, "y": 50, "width": 300, "height": 200 }
        });
        assert_eq!(
            ScreenshotTool::maim_arguments(&args, std::path::Path::new("/tmp/out.png")),
            vec!["-g", "100,50,300,200", "/tmp/out.png"]
        );
    }

    /// `import -crop` wants `wxh+x+y`, so the numbers are reordered.
    ///
    /// Passing the comma form would crop a rectangle whose width is the x
    /// origin — plausible-looking output from a request that was never honoured.
    #[test]
    fn import_is_given_the_region_in_its_own_geometry() {
        let args = serde_json::json!({
            "region": { "x": 100, "y": 50, "width": 300, "height": 200 }
        });
        assert_eq!(
            ScreenshotTool::import_arguments(&args, std::path::Path::new("/tmp/out.png")),
            vec!["-window", "root", "-crop", "300x200+100+50", "/tmp/out.png"]
        );
    }

    /// No region means no crop flag, not a zero-size one.
    #[test]
    fn a_whole_screen_capture_passes_no_crop_flag() {
        let empty = serde_json::json!({});
        let maim = ScreenshotTool::maim_arguments(&empty, std::path::Path::new("/tmp/o.png"));
        assert!(!maim.iter().any(|a| a == "-g"), "{maim:?}");
        let import = ScreenshotTool::import_arguments(&empty, std::path::Path::new("/tmp/o.png"));
        assert!(!import.iter().any(|a| a == "-crop"), "{import:?}");
    }
}
