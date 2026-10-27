//! Image Tools — Image Viewing and AI Generation
//!
//! tools for:
//! - `image`: View image file info (dimensions, format, size)
//! - `image_generate`: Generate images via external AI APIs (DALL-E, Stable
//!   Diffusion)

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::info;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::process_runner::ProcessRequest;
use crate::tools::sdk::ToolCapabilities;

#[allow(clippy::unwrap_used)] // Static regex with known-valid pattern.
static RE_IMAGE_DIMS: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(\d+)\s*x\s*(\d+)").unwrap());

// ── Image Tool (view / inspect) ─────────────────────────────────────────────

/// Image inspection tool
pub struct ImageTool;

impl ImageTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ImageTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct ImageArgs {
    path: String,
}

#[async_trait]
impl Tool for ImageTool {
    fn name(&self) -> &str {
        "image"
    }

    fn description(&self) -> &str {
        "View an image file: dimensions, format, file size, and — when the build supports it — \
         the text found in the image via local OCR, in reading order. Use this tool when the \
         user asks to view, read, or inspect an image, or when an answer depends on what an \
         image or screenshot says (查看/读取图片内容). Supports common formats: PNG, JPG, GIF, \
         WebP, BMP, TIFF. OCR reads text only; it does not describe pictures or photos."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the image file"
                }
            },
            "required": ["path"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["media".to_string(), "image".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: ImageArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let path = context.resolve_path(std::path::Path::new(&args.path));

        if !path.exists() {
            return Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Image file not found: {}", args.path)),
                data: None,
                execution_time: start.elapsed(),
            });
        }

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Cannot read image file: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let file_size = metadata.len();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("unknown")
            .to_lowercase();

        let format = match ext.as_str() {
            "png" => "PNG",
            "jpg" | "jpeg" => "JPEG",
            "gif" => "GIF",
            "webp" => "WebP",
            "svg" => "SVG",
            "bmp" => "BMP",
            "tiff" | "tif" => "TIFF",
            _ => "Unknown",
        };

        // Try to detect dimensions using the `file` command or image header parsing
        let mut width = None::<u32>;
        let mut height = None::<u32>;

        // Try file command for basic dimensions
        if let Ok(output) = crate::tools::process_runner::run(&ProcessRequest::argv(&[
            "file",
            path.to_str().unwrap_or(""),
        ]))
        .await
        {
            let stdout = output.stdout_string();
            // Parse "640 x 480" from file output
            if let Some(caps) = RE_IMAGE_DIMS.captures(&stdout) {
                width = caps.get(1).and_then(|m| m.as_str().parse().ok());
                height = caps.get(2).and_then(|m| m.as_str().parse().ok());
            }
        }

        // Fallback: try parsing PNG/JPEG headers directly
        if width.is_none() && (ext == "png" || ext == "jpg" || ext == "jpeg") {
            if let Ok(data) = tokio::fs::read(&path).await {
                if ext == "png" && data.len() > 24 {
                    // PNG: width at bytes 16-19, height at 20-23 (big-endian)
                    width = Some(u32::from_be_bytes([data[16], data[17], data[18], data[19]]));
                    height = Some(u32::from_be_bytes([data[20], data[21], data[22], data[23]]));
                }
            }
        }

        let dim_str = match (width, height) {
            (Some(w), Some(h)) => format!("{}x{}", w, h),
            _ => "unknown dimensions".to_string(),
        };

        let mut output = format!(
            "{} image: {} ({} bytes, {})",
            format,
            path.file_name().unwrap_or_default().to_string_lossy(),
            file_size,
            dim_str
        );

        // Text extraction is the only way this tool answers a question about
        // an image's contents: the model has no pixel channel for tool
        // results. Run it here rather than making the agent hunt for an OCR
        // utility it cannot install.
        #[cfg(feature = "vision")]
        let ocr_text: Option<String> =
            match crate::computer::vision::ocr_file::ocr_image_file(&path).await {
                Ok(ocr) if !ocr.text.trim().is_empty() => {
                    let text = truncate_ocr(ocr.text.trim());
                    output.push_str("\n\nText found in the image (OCR, reading order):\n");
                    output.push_str(&text);
                    if !ocr.is_reliable() {
                        // Low-confidence OCR on a busy page produces plausible
                        // fragments in the wrong order. Say so, or the model
                        // will treat them as quoted evidence.
                        output.push_str(&format!(
                            "\n\n[OCR confidence is low ({:.0}% mean over {} block(s)); treat \
                             this text as a rough hint, not as quoted content. Verify anything \
                             you rely on.]",
                            ocr.mean_confidence * 100.0,
                            ocr.blocks
                        ));
                    }
                    Some(text)
                }
                Ok(_) => {
                    output.push_str(
                        "\n\nOCR ran and found no text in this image. It may be a photo, diagram, \
                     or picture without machine-readable text — reason about it from other \
                     evidence instead.",
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!("OCR failed for {}: {}", path.display(), e);
                    output.push_str(
                        "\n\nText extraction (OCR) is unavailable for this image; only file \
                     metadata is reported.",
                    );
                    None
                }
            };

        #[cfg(not(feature = "vision"))]
        let ocr_text: Option<String> = {
            output.push_str("\n\nThis build has no OCR support; only file metadata is reported.");
            None
        };

        let mut info = serde_json::json!({
            "path": path.to_string_lossy(),
            "format": format,
            "file_size": file_size,
            "file_size_human": format_file_size(file_size),
            "width": width,
            "height": height,
        });
        if let Some(text) = &ocr_text {
            info["text"] = serde_json::json!(text);
        }

        Ok(ToolExecutionResult {
            success: true,
            output,
            error: None,
            data: Some(info),
            execution_time: start.elapsed(),
        })
    }
}

/// OCR text is model input, so it is capped like any other tool output.
#[cfg(feature = "vision")]
const MAX_OCR_CHARS: usize = 20_000;

/// Cap OCR text at [`MAX_OCR_CHARS`], keeping the beginning.
#[cfg(feature = "vision")]
fn truncate_ocr(text: &str) -> String {
    if text.chars().count() <= MAX_OCR_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(MAX_OCR_CHARS).collect();
    cut.push_str("\n… [OCR output truncated]");
    cut
}

fn format_file_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut size = size as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}

// ── Image Generate Tool ─────────────────────────────────────────────────────

/// AI image generation tool
pub struct ImageGenerateTool;

impl ImageGenerateTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ImageGenerateTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct ImageGenerateArgs {
    prompt: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    style: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

#[async_trait]
impl Tool for ImageGenerateTool {
    fn name(&self) -> &str {
        "image_generate"
    }

    fn description(&self) -> &str {
        "Generate an image from a text description using an AI image generation API (e.g., DALL-E, \
         Stable Diffusion). Use this tool when the user asks to generate, create, draw, or produce \
         an image (生成/创建/画图). Requires an API key configured in the environment."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text description of the image to generate"
                },
                "size": {
                    "type": "string",
                    "enum": ["256x256", "512x512", "1024x1024", "1792x1024", "1024x1792"],
                    "default": "1024x1024",
                    "description": "Image dimensions"
                },
                "style": {
                    "type": "string",
                    "enum": ["vivid", "natural"],
                    "default": "vivid",
                    "description": "Image style (DALL-E 3)"
                },
                "output": {
                    "type": "string",
                    "description": "Output file path (default: auto-generated in working dir)"
                }
            },
            "required": ["prompt"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["media".to_string(), "ai".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: ImageGenerateArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        // The configured provider, not an environment variable of this tool's
        // own: media used to reach OpenAI directly, which meant it ignored the
        // provider the agent was actually configured with — including its
        // credential and its fallback chain.
        let Some(provider) = context.media_provider().cloned() else {
            return Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Image generation needs a configured provider, and this context has none. \
                     Run it in an agent turn, or configure a provider."
                        .to_string(),
                ),
                data: None,
                execution_time: start.elapsed(),
            });
        };

        let size = args.size.as_deref().unwrap_or("1024x1024");

        let mut request = crate::providers::media::MediaRequest::new(
            crate::providers::media::MediaKind::Image,
            args.prompt.clone(),
        );
        request.model = Some("dall-e-3".to_string());
        request.size = Some(size.to_string());
        request.style = args.style.clone();

        let response = match provider.generate_media(request).await {
            Ok(response) => response,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Image generation failed: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        // The provider may answer with a link or with the image itself. Both are
        // saved the same way, so which one arrived is nobody else's problem.
        let (bytes, url) = match response.output {
            crate::providers::media::MediaOutput::Bytes { data, .. } => (Some(data), None),
            crate::providers::media::MediaOutput::Url(url) => {
                // Downloading is best-effort: a link the agent can still hand on
                // is a success even when this process cannot fetch it.
                let bytes = match reqwest::get(&url).await {
                    Ok(resp) => resp.bytes().await.ok().map(|bytes| bytes.to_vec()),
                    Err(_) => None,
                };
                (bytes, Some(url))
            }
            crate::providers::media::MediaOutput::Text(_) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some("the provider answered an image request with text".to_string()),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        match (bytes, url) {
            (Some(bytes), url) => {
                let output_path = if let Some(out) = args.output.clone() {
                    context.resolve_path(std::path::Path::new(&out))
                } else {
                    context
                        .workspace_root()
                        .join(format!("generated_image_{}.png", uuid::Uuid::new_v4()))
                };

                if let Err(e) = tokio::fs::write(&output_path, &bytes).await {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to save image: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                info!("Image generated and saved: {:?}", output_path);
                Ok(ToolExecutionResult {
                    success: true,
                    output: format!("Image generated: {}", output_path.display()),
                    error: None,
                    data: Some(serde_json::json!({
                        "url": url,
                        "local_path": output_path.to_string_lossy(),
                        "prompt": args.prompt,
                        "size": size,
                    })),
                    execution_time: start.elapsed(),
                })
            }
            (None, Some(url)) => Ok(ToolExecutionResult {
                success: true,
                output: format!("Image generated (URL only): {}", url),
                error: None,
                data: Some(serde_json::json!({
                    "url": url,
                    "prompt": args.prompt,
                    "size": size,
                })),
                execution_time: start.elapsed(),
            }),
            (None, None) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some("image generation returned neither an image nor a URL".to_string()),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_args_parsing() {
        let args: ImageArgs = serde_json::from_value(serde_json::json!({
            "path": "/tmp/test.png"
        }))
        .unwrap();
        assert_eq!(args.path, "/tmp/test.png");
    }

    #[test]
    fn test_format_file_size() {
        assert_eq!(format_file_size(0), "0.0 B");
        assert_eq!(format_file_size(512), "512.0 B");
        assert_eq!(format_file_size(1024), "1.0 KB");
        assert_eq!(format_file_size(1536), "1.5 KB");
        assert_eq!(format_file_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_file_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn test_image_tool_name_and_schema() {
        let tool = ImageTool::new();
        assert_eq!(tool.name(), "image");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_image_generate_tool_name_and_schema() {
        let tool = ImageGenerateTool::new();
        assert_eq!(tool.name(), "image_generate");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_image_generate_args_parsing() {
        let args: ImageGenerateArgs = serde_json::from_value(serde_json::json!({
            "prompt": "a cat"
        }))
        .unwrap();
        assert_eq!(args.prompt, "a cat");
        assert_eq!(args.size, None);
        assert_eq!(args.style, None);

        let args2: ImageGenerateArgs = serde_json::from_value(serde_json::json!({
            "prompt": "a dog",
            "size": "1024x1024",
            "style": "vivid"
        }))
        .unwrap();
        assert_eq!(args2.size, Some("1024x1024".to_string()));
        assert_eq!(args2.style, Some("vivid".to_string()));
    }

    #[tokio::test]
    async fn test_image_tool_missing_file() {
        let tool = ImageTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(serde_json::json!({ "path": "/nonexistent/path/image.png" }), &ctx)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_image_tool_invalid_args() {
        let tool = ImageTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid arguments"));
    }

    /// With no provider on the context there is no way to generate anything —
    /// and the tool says that, rather than falling back to reading an
    /// environment variable of its own, which is what it used to do.
    #[tokio::test]
    async fn test_image_generate_tool_without_a_provider() {
        let tool = ImageGenerateTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(serde_json::json!({ "prompt": "a cat" }), &ctx)
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("needs a configured provider"), "{error}");
        assert!(
            !error.contains("API key"),
            "the tool no longer asks for an environment variable of its own: {error}"
        );
    }
}
