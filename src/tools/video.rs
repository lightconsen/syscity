//! `video_generate` — text (or an image) to video, through the configured
//! provider.
//!
//! Video is the first media kind that is not one request: the vendor accepts a
//! job, reports progress, and serves the result when it is done, so the wait
//! lives in the provider and this tool only decides where to put the bytes.
//!
//! `video_analyze` is deliberately not here. Watching a video is a *vision*
//! task — frames through a multimodal model — not a media-generation one, and
//! folding it into this abstraction would put two unrelated capabilities behind
//! one request type.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::providers::media::{MediaKind, MediaOutput, MediaRequest};
use crate::tools::util::create_schema;
use crate::tools::{Tool, ToolContext, ToolExecutionResult};

/// `video_generate`: ask the configured provider for a video.
pub struct VideoGenerateTool;

impl VideoGenerateTool {
    /// A new tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for VideoGenerateTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct VideoArgs {
    /// What the video should show.
    prompt: String,
    /// How long it should be, in seconds.
    #[serde(default)]
    duration: Option<u32>,
    /// Output aspect ratio, e.g. `16:9`.
    #[serde(default)]
    aspect_ratio: Option<String>,
    /// Output resolution, e.g. `720p`.
    #[serde(default)]
    resolution: Option<String>,
    /// Where to write the file; defaults to the workspace.
    #[serde(default)]
    output: Option<String>,
    /// A model override, when the provider offers more than one.
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Tool for VideoGenerateTool {
    fn name(&self) -> &str {
        "video_generate"
    }

    fn description(&self) -> &str {
        "Generate a video from a text description. Use this tool when the user asks for a video, \
         an animation, or a moving clip (视频生成/文生视频). Uses the configured model provider; \
         generation takes minutes, and the result is saved to a file."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Generate a video",
            serde_json::json!({
                "prompt": {
                    "type": "string",
                    "description": "What the video should show: subject, motion, style, camera movement."
                },
                "duration": {
                    "type": "integer",
                    "description": "Desired length in seconds. The provider clamps to the range its model supports."
                },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Output aspect ratio, e.g. 16:9 or 9:16."
                },
                "resolution": {
                    "type": "string",
                    "description": "Output resolution, e.g. 720p or 1080p."
                },
                "output": {
                    "type": "string",
                    "description": "Where to write the video. Defaults to the workspace."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override. The configured default is used when omitted."
                }
            }),
            vec!["prompt"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();

        let args: VideoArgs = match serde_json::from_value(args) {
            Ok(args) => args,
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

        let Some(provider) = context.media_provider().cloned() else {
            return Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Video generation needs a configured provider, and this context has none. \
                     Run it in an agent turn, or configure a provider."
                        .to_string(),
                ),
                data: None,
                execution_time: start.elapsed(),
            });
        };

        let mut request = MediaRequest::new(MediaKind::Video, args.prompt.clone());
        request.model = args.model.clone();
        request.duration = args.duration;
        request.aspect_ratio = args.aspect_ratio.clone();
        request.resolution = args.resolution.clone();

        let response = match provider.generate_media(request).await {
            Ok(response) => response,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Video generation failed: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let bytes = match response.output {
            MediaOutput::Bytes { data, .. } => data,
            MediaOutput::Url(url) => {
                // A provider that hands back a link has still generated the
                // video; keeping the URL is more useful than failing.
                info!("Video generated at {} (not downloaded)", url);
                return Ok(ToolExecutionResult {
                    success: true,
                    output: format!("Video generated: {}", url),
                    error: None,
                    data: Some(serde_json::json!({
                        "url": url,
                        "prompt": args.prompt,
                        "provider": provider.name(),
                    })),
                    execution_time: start.elapsed(),
                });
            }
            MediaOutput::Text(_) => {
                warn!("a video provider answered with text");
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some("the provider answered a video request with text".to_string()),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let output_path = match args.output {
            Some(out) => context.resolve_path(std::path::Path::new(&out)),
            None => context
                .workspace_root()
                .join(format!("generated_video_{}.mp4", uuid::Uuid::new_v4())),
        };

        if let Err(e) = tokio::fs::write(&output_path, &bytes).await {
            return Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to save video: {}", e)),
                data: None,
                execution_time: start.elapsed(),
            });
        }

        info!("Video generated and saved: {:?}", output_path);
        Ok(ToolExecutionResult {
            success: true,
            output: format!("Video generated: {}", output_path.display()),
            error: None,
            data: Some(serde_json::json!({
                "local_path": output_path.to_string_lossy(),
                "prompt": args.prompt,
                "duration": args.duration,
                "aspect_ratio": args.aspect_ratio,
                "resolution": args.resolution,
                "provider": provider.name(),
            })),
            execution_time: start.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::media::MediaResponse;
    use crate::providers::Provider;
    use std::sync::Arc;

    /// A provider that answers video requests with fixed bytes.
    struct VideoProvider {
        answer: Result<MediaResponse, String>,
    }

    #[async_trait]
    impl Provider for VideoProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn default_model(&self) -> &str {
            "stub-model"
        }
        fn supports_tools(&self) -> bool {
            false
        }
        fn max_context(&self) -> usize {
            0
        }
        async fn complete(
            &self,
            _request: crate::providers::CompletionRequest,
        ) -> crate::Result<crate::providers::CompletionResponse> {
            Err(crate::error::SyscityError::Unsupported("stub".into()))
        }
        async fn stream(
            &self,
            _request: crate::providers::CompletionRequest,
        ) -> crate::Result<crate::providers::CompletionStream> {
            Err(crate::error::SyscityError::Unsupported("stub".into()))
        }
        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }
        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }

        async fn generate_media(&self, _request: MediaRequest) -> crate::Result<MediaResponse> {
            match &self.answer {
                Ok(response) => Ok(response.clone()),
                Err(reason) => Err(crate::error::SyscityError::ExternalService {
                    source: reason.clone(),
                    cause: None,
                }),
            }
        }
    }

    fn context_with(answer: Result<MediaResponse, String>) -> (ToolContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp workspace");
        let ctx = ToolContext::new("user", "conv")
            .with_workspace_root(dir.path().to_path_buf())
            .with_media_provider(Arc::new(VideoProvider { answer }));
        (ctx, dir)
    }

    /// The bytes the provider produced land in a file, and the result says where.
    #[tokio::test]
    async fn a_generated_video_is_written_to_the_workspace() {
        let (ctx, _dir) = context_with(Ok(MediaResponse::bytes(
            b"ftypisom".to_vec(),
            Some("video/mp4".to_string()),
        )));

        let result = VideoGenerateTool::new()
            .execute(serde_json::json!({ "prompt": "a fox running" }), &ctx)
            .await
            .expect("the tool answers");

        assert!(result.success, "{result:?}");
        let path = result.data.as_ref().expect("data")["local_path"]
            .as_str()
            .expect("a path");
        let written = tokio::fs::read(path).await.expect("the file exists");
        assert_eq!(written, b"ftypisom");
    }

    /// A provider that hands back a link is still a success — the video exists,
    /// it is just not local.
    #[tokio::test]
    async fn a_video_url_is_returned_rather_than_dropped() {
        let (ctx, _dir) = context_with(Ok(MediaResponse::url("https://example.invalid/v.mp4")));

        let result = VideoGenerateTool::new()
            .execute(serde_json::json!({ "prompt": "a fox" }), &ctx)
            .await
            .expect("the tool answers");

        assert!(result.success);
        assert!(result.output.contains("https://example.invalid/v.mp4"));
    }

    /// With no provider there is nothing to generate anything with, and the tool
    /// says so rather than reaching for an environment variable.
    #[tokio::test]
    async fn without_a_provider_the_tool_says_so() {
        let ctx = ToolContext::new("user", "conv");
        let result = VideoGenerateTool::new()
            .execute(serde_json::json!({ "prompt": "a fox" }), &ctx)
            .await
            .expect("the tool answers");

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("needs a configured provider"));
    }

    /// A provider failure is reported, not turned into an empty success.
    #[tokio::test]
    async fn a_provider_failure_is_reported() {
        let (ctx, _dir) = context_with(Err("the job expired".to_string()));

        let result = VideoGenerateTool::new()
            .execute(serde_json::json!({ "prompt": "a fox" }), &ctx)
            .await
            .expect("the tool answers");

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("the job expired"));
    }
}
