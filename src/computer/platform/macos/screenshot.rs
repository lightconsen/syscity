//! macOS screenshot tool using `screencapture`.

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

use crate::computer::screenshot_encoder::maybe_encode_screenshot;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

/// Take screenshots on macOS using the built-in `screencapture` utility.
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

    /// Extract image dimensions from raw PNG or JPEG bytes by parsing headers.
    fn image_dimensions(data: &[u8]) -> (u32, u32) {
        // PNG: signature + IHDR chunk — width at offset 16, height at offset 20
        if data.len() >= 24 && data[..8] == [137, 80, 78, 71, 13, 10, 26, 10] {
            let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
            let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
            return (w, h);
        }
        // JPEG: find SOF0 marker (0xFF 0xC0)
        if data.len() > 4 && data[0] == 0xFF && data[1] == 0xD8 {
            let mut i = 2;
            while i + 4 < data.len() {
                if data[i] == 0xFF {
                    let marker = data[i + 1];
                    if marker == 0xC0 {
                        // SOF0: next 2 bytes are length, then 1 byte precision,
                        // then 2 bytes height (BE), 2 bytes width (BE)
                        if i + 9 < data.len() {
                            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]);
                            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]);
                            return (w as u32, h as u32);
                        }
                    }
                    if (0xD0..=0xD9).contains(&marker) {
                        i += 2; // markers with no length
                        continue;
                    }
                    if i + 2 < data.len() {
                        let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                        i += 2 + seg_len;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        }
        (0, 0)
    }

    /// The argv `screencapture` is given for a whole tool invocation.
    ///
    /// This is the composition `execute` runs — parse the caller's arguments,
    /// then build the command — kept as one function so a test can assert the
    /// *join* rather than its two halves. Testing the parser and the flag
    /// builder separately would have passed while `execute` ignored the region
    /// entirely, which is exactly the defect this replaces.
    fn invocation_arguments(args: &Value, destination: &std::path::Path) -> Vec<String> {
        let region = Self::region_argument(args);
        Self::capture_arguments(region.as_deref(), destination)
    }

    /// The argv `screencapture` is given for one capture.
    ///
    /// Split out from the command so the flags can be asserted without a
    /// display: a machine without screen-recording permission cannot take a
    /// screenshot to inspect, and a test that silently skips there would prove
    /// nothing about the flag it is meant to check.
    fn capture_arguments(region: Option<&str>, destination: &std::path::Path) -> Vec<String> {
        let mut args = vec!["-x".to_string(), "-C".to_string()];
        if let Some(region) = region {
            // `-R` crops in the display's own coordinate space, which is the
            // space `Rect` is in. Cropping here rather than resizing the full
            // capture afterwards is what makes a close-up a close-up: the
            // pixels are the screen's resolution, not a zoom of a picture that
            // already lost the small controls to the detector's 640-wide pass.
            args.push("-R".to_string());
            args.push(region.to_string());
        }
        args.push(destination.display().to_string());
        args
    }

    /// The region to crop to, in `screencapture -R`'s `x,y,width,height` form.
    ///
    /// The shape rules live in [`crate::computer::platform::common::screenshot_region`],
    /// shared with the other platforms' tools, so a region cannot mean one thing
    /// to the macOS tool and another to the x11 one.
    fn region_argument(args: &Value) -> Option<String> {
        crate::computer::platform::common::screenshot_region_commas(args)
    }
}

#[async_trait]
impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "macos_screenshot"
    }

    fn description(&self) -> &str {
        "Take a screenshot on macOS using `screencapture`. Returns a base64-encoded PNG image. Use \
         when you need to visually verify the current screen state."
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::computer::platform::capabilities::observe(&["computer", "macos", "screenshot"])
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Take a screenshot",
            serde_json::json!({
                "display": {
                    "type": "integer",
                    "description": "Display number to capture (default: main display)",
                    "default": 1
                },
                "window": {
                    "type": "string",
                    "description": "Optional window name to capture instead of full screen"
                },
                "region": {
                    "type": "object",
                    "description": "Optional region to capture, as {\"x\", \"y\", \"width\", \
                                    \"height\"} in screen points. The capture is cropped by \
                                    `screencapture -R`, so the pixels are the screen's own \
                                    resolution rather than a zoom of the full screen.",
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
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let _t0 = std::time::Instant::now();

        // ── Determine output path ───────────────────────────────────────
        // Save in workspace/files/ for temporary files.
        let screenshot_dir = context.workspace_root().join("files");
        tokio::fs::create_dir_all(&screenshot_dir)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to create screenshot dir: {}", screenshot_dir.display()),
                details: e.to_string(),
            })?;

        let final_path =
            screenshot_dir.join(format!("screenshot_{}.png", crate::utils::ms_timestamp()));

        // Use a random suffix for the temp staging path to avoid collisions
        // when two calls happen in the same millisecond (e.g., parallel tests).
        let suffix: u16 = rand::random();
        let temp_path = std::env::temp_dir().join(format!(
            "screenshot_{}_{}.tmp",
            crate::utils::ms_timestamp(),
            suffix,
        ));

        info!("Taking screenshot: {} (final: {})", temp_path.display(), final_path.display());

        let command_args = Self::invocation_arguments(&args, &temp_path);
        let mut command = Command::new("screencapture");
        command.args(&command_args);

        let result = timeout(Duration::from_secs(15), command.output()).await;

        match result {
            Ok(Ok(output)) if output.status.success() => {
                info!("screencapture done in {:?}", _t0.elapsed());

                // Compress the screenshot using the cross-platform ScreenshotEncoder.
                let encoded_path = maybe_encode_screenshot(&temp_path).await;
                let format = if encoded_path
                    .extension()
                    .map(|e| e == "jpg" || e == "jpeg")
                    .unwrap_or(false)
                {
                    "jpeg"
                } else {
                    "png"
                };

                // Move/copy the encoded file to the final path in workspace.
                if encoded_path.as_os_str() != final_path.as_os_str() {
                    if let Err(e) = tokio::fs::copy(&encoded_path, &final_path).await {
                        warn!("Failed to copy screenshot to workspace: {}", e);
                        // fall through — still have the encoded file
                    }
                }

                let bytes = tokio::fs::read(&encoded_path).await.map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: format!("Failed to read screenshot: {}", encoded_path.display()),
                        details: e.to_string(),
                    }
                })?;

                // Extract dimensions from image header.
                let (width, height) = Self::image_dimensions(&bytes);

                // Clean up temp file(s)
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

                info!(
                    "Screenshot tool: read+encode done in {:?}, final width={} height={} size={}",
                    _t0.elapsed(),
                    width,
                    height,
                    bytes.len()
                );

                Ok(ToolExecutionResult::success(format!(
                    "Screenshot captured ({}x{}, {} bytes, file: {})",
                    width,
                    height,
                    bytes.len(),
                    final_path.display(),
                ))
                .with_data(serde_json::json!({
                    "image_base64": b64,
                    "data_url": data_url,
                    "file_path": final_path.to_string_lossy().to_string(),
                    "width": width,
                    "height": height,
                    "format": format,
                    "size": bytes.len()
                })))
            }
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("screencapture failed: {}", stderr);
                Ok(ToolExecutionResult::error(format!("screencapture failed: {}", stderr)))
            }
            Ok(Err(e)) => {
                Ok(ToolExecutionResult::error(format!("Failed to run screencapture: {}", e)))
            }
            Err(_) => Ok(ToolExecutionResult::error("screencapture timed out".to_string())),
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        cfg!(target_os = "macos")
    }

    fn skip_content_filter(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use std::sync::Arc;

    #[test]
    fn test_screenshot_tool_creation() {
        let tool = ScreenshotTool::new();
        assert_eq!(tool.name(), "macos_screenshot");
        assert!(tool.description().contains("screenshot"));
    }

    #[tokio::test]
    async fn test_screenshot_timing_direct() {
        let tool = ScreenshotTool::new();
        let ctx = ToolContext::default();

        let _t = std::time::Instant::now();
        let result = tool.execute(serde_json::json!({}), &ctx).await;
        let elapsed = _t.elapsed();
        eprintln!("[TIMING_TEST] ScreenshotTool::execute() took {:?}", elapsed);
        assert!(result.is_ok(), "screenshot should succeed: {:?}", result.err());
        let r = result.unwrap();
        let keys: Vec<String> = r
            .data
            .as_ref()
            .and_then(|d| d.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        eprintln!("[TIMING_TEST] screenshot result data keys: {:?}", keys);
        if let Some(b64) = r
            .data
            .as_ref()
            .and_then(|d| d.get("image_base64"))
            .and_then(|v| v.as_str())
        {
            eprintln!("[TIMING_TEST] base64 length: {} chars", b64.len());
        }
    }

    #[tokio::test]
    async fn test_screenshot_timing_via_registry() {
        let ctx = ToolContext::default();
        let mut registry = ToolRegistry::new();

        // Register the screenshot tool into the registry
        registry.register(Box::new(ScreenshotTool::new()));

        let _t = std::time::Instant::now();
        let result = registry
            .execute("macos_screenshot", serde_json::json!({}), &ctx)
            .await;
        let elapsed = _t.elapsed();
        eprintln!(
            "[TIMING_TEST] Registry::execute(macos_screenshot) (no filter) took {:?}",
            elapsed
        );
        assert!(result.is_some(), "tool should be found");
        let inner = result.unwrap();
        assert!(inner.is_ok(), "screenshot should succeed: {:?}", inner.err());
    }

    #[tokio::test]
    async fn test_screenshot_timing_with_filter() {
        let ctx = ToolContext::default();
        use crate::security::content_filter::ContentFilter;
        let filter = Arc::new(ContentFilter::default());
        let mut registry = ToolRegistry::new().with_content_filter(filter);

        registry.register(Box::new(ScreenshotTool::new()));

        let _t = std::time::Instant::now();
        let result = registry
            .execute("macos_screenshot", serde_json::json!({}), &ctx)
            .await;
        let elapsed = _t.elapsed();
        eprintln!(
            "[TIMING_TEST] Registry::execute(macos_screenshot) (with content_filter) took {:?}",
            elapsed
        );
        assert!(result.is_some(), "tool should be found");
        let inner = result.unwrap();
        assert!(inner.is_ok(), "screenshot should succeed: {:?}", inner.err());
    }

    /// The adapter's structured region becomes `screencapture -R`'s argument.
    ///
    /// This is the join that was missing: the computer adapter has always sent
    /// a region, and the tool always ignored it, so every close-up silently
    /// returned the whole screen — and the caller then mapped those full-screen
    /// pixels as if they were a crop.
    #[test]
    fn the_adapters_region_becomes_a_crop_argument() {
        let args = serde_json::json!({
            "region": { "x": 100, "y": 50, "width": 300, "height": 200 }
        });
        assert_eq!(ScreenshotTool::region_argument(&args).as_deref(), Some("100,50,300,200"));
        // And it reaches the command line the tool really runs, before the
        // destination. Asserted through `invocation_arguments` rather than
        // `capture_arguments`, because the halves each being right is what
        // let this defect live: the parser was fine and the flag builder was
        // fine, and `execute` used neither.
        assert_eq!(
            ScreenshotTool::invocation_arguments(&args, std::path::Path::new("/tmp/out.png")),
            vec!["-x", "-C", "-R", "100,50,300,200", "/tmp/out.png"],
            "the crop flag must be on the command line, not merely parsed"
        );
    }

    /// A whole-screen capture passes no crop flag at all.
    ///
    /// Not `-R 0,0,W,H` with a guessed size: the tool does not know the display
    /// dimensions here, so the honest whole-screen request is the absence of a
    /// rect, which is what `screencapture` treats as the full screen.
    #[test]
    fn a_whole_screen_capture_names_no_region() {
        let argv = ScreenshotTool::invocation_arguments(
            &serde_json::json!({}),
            std::path::Path::new("/tmp/out.png"),
        );
        assert!(!argv.iter().any(|a| a == "-R"), "no region means no crop: {argv:?}");
    }

    /// The string form a model writes by hand is accepted too.
    #[test]
    fn a_region_string_is_understood() {
        let args = serde_json::json!({ "region": " 10, 20 , 30,40 " });
        assert_eq!(ScreenshotTool::region_argument(&args).as_deref(), Some("10,20,30,40"));
    }

    /// No region means the whole screen, not a refusal.
    #[test]
    fn a_screenshot_without_a_region_is_not_cropped() {
        assert_eq!(ScreenshotTool::region_argument(&serde_json::json!({})), None);
    }

    /// A region that cannot be cropped is refused rather than guessed.
    ///
    /// `-R` with a bogus value captures something the caller never asked for.
    /// A close-up of an empty rectangle is a wrong answer delivered
    /// confidently, which is the one outcome worth avoiding here.
    #[test]
    fn a_malformed_region_is_refused() {
        for bad in [
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": 0, "height": 10 } }),
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": -5, "height": 10 } }),
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": 10 } }),
            serde_json::json!({ "region": "0,0,ten,10" }),
            serde_json::json!({ "region": "0,0,0,10" }),
            serde_json::json!({ "region": "0,0,10" }),
        ] {
            assert_eq!(ScreenshotTool::region_argument(&bad), None, "must refuse: {bad}");
        }
    }
}
