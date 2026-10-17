//! STT Tool — Speech-to-Text
//!
//! tool for transcribing audio to text.
//! Uses OpenAI Whisper API as the backend.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::info;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::sdk::ToolCapabilities;

/// Speech-to-text tool
pub struct SttTool;

impl SttTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SttTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct SttArgs {
    /// Path to the audio file to transcribe
    audio: String,
    /// Whisper model name (default: whisper-1)
    #[serde(default)]
    model: Option<String>,
    /// Language code (e.g., "en", "zh") — auto-detected if not set
    #[serde(default)]
    language: Option<String>,
    /// Sampling temperature (0.0 - 1.0)
    #[serde(default)]
    temperature: Option<f32>,
    /// Optional context prompt to guide the transcription
    #[serde(default)]
    prompt: Option<String>,
}

#[async_trait]
impl Tool for SttTool {
    fn name(&self) -> &str {
        "stt"
    }

    fn description(&self) -> &str {
        "Transcribe audio to text (speech-to-text) using OpenAI Whisper API. Use this tool when \
         the user asks to transcribe, convert speech to text, or recognize audio content \
         (转录/语音识别/音频转文字). Requires OPENAI_API_KEY. Accepts common audio formats: mp3, \
         wav, m4a, ogg, flac, webm, etc."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "audio": {
                    "type": "string",
                    "description": "Path to the audio file to transcribe"
                },
                "model": {
                    "type": "string",
                    "description": "Whisper model name",
                    "default": "whisper-1"
                },
                "language": {
                    "type": "string",
                    "description": "Language code (e.g., 'en', 'zh'). Auto-detected if omitted."
                },
                "temperature": {
                    "type": "number",
                    "description": "Sampling temperature (0.0 - 1.0)"
                },
                "prompt": {
                    "type": "string",
                    "description": "Optional context prompt to guide the transcription"
                }
            },
            "required": ["audio"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            read_only: true,
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["media".to_string(), "audio".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();

        let args: SttArgs = match serde_json::from_value(args) {
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

        let audio_path = std::path::PathBuf::from(&args.audio);

        // Check file exists
        if !audio_path.exists() {
            return Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Audio file not found: {}", audio_path.display())),
                data: None,
                execution_time: start.elapsed(),
            });
        }

        // The configured provider — the same one the agent uses, with its
        // credential and fallback chain — rather than a second read of
        // `OPENAI_API_KEY`.
        let Some(provider) = context.media_provider().cloned() else {
            return Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(
                    "Audio transcription needs a configured provider, and this context has none. \
                     Run it in an agent turn, or configure a provider."
                        .to_string(),
                ),
                data: None,
                execution_time: start.elapsed(),
            });
        };

        let audio_bytes = match tokio::fs::read(&audio_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read audio file: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        // The container is inferred from the extension, so the file's own
        // extension is what the provider should be told.
        let extension = audio_path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_else(|| "mp3".to_string());

        let mut request = crate::providers::media::MediaRequest::new(
            crate::providers::media::MediaKind::Transcription,
            args.prompt.clone().unwrap_or_default(),
        );
        request.model = args.model.clone();
        request.input = Some(audio_bytes);
        request.format = Some(extension);
        request.language = args.language.clone();
        request.temperature = args.temperature.map(f64::from);

        return match provider.generate_media(request).await {
            Ok(response) => match response.output {
                crate::providers::media::MediaOutput::Text(text) => {
                    info!("STT transcription completed via {}", provider.name());
                    Ok(ToolExecutionResult {
                        success: true,
                        output: text.clone(),
                        error: None,
                        data: Some(serde_json::json!({
                            "text": text,
                            "model": args.model.clone().unwrap_or_else(|| "whisper-1".to_string()),
                            "provider": provider.name(),
                            "audio_file": audio_path.to_string_lossy(),
                        })),
                        execution_time: start.elapsed(),
                    })
                }
                other => Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "the provider answered a transcription with something other than text: {other:?}"
                    )),
                    data: None,
                    execution_time: start.elapsed(),
                }),
            },
            Err(e) => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Transcription failed: {}", e)),
                data: None,
                execution_time: start.elapsed(),
            }),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stt_args_defaults() {
        let args: SttArgs = serde_json::from_value(serde_json::json!({
            "audio": "/tmp/test.mp3"
        }))
        .unwrap();
        assert_eq!(args.audio, "/tmp/test.mp3");
        assert_eq!(args.model, None);
        assert_eq!(args.language, None);
        assert_eq!(args.temperature, None);
        assert_eq!(args.prompt, None);
    }

    #[test]
    fn test_stt_args_custom() {
        let args: SttArgs = serde_json::from_value(serde_json::json!({
            "audio": "/tmp/speech.wav",
            "model": "whisper-1",
            "language": "zh",
            "temperature": 0.5,
            "prompt": "technical conversation"
        }))
        .unwrap();
        assert_eq!(args.audio, "/tmp/speech.wav");
        assert_eq!(args.model, Some("whisper-1".to_string()));
        assert_eq!(args.language, Some("zh".to_string()));
        assert_eq!(args.temperature, Some(0.5));
        assert_eq!(args.prompt, Some("technical conversation".to_string()));
    }

    #[test]
    fn test_stt_tool_name_and_schema() {
        let tool = SttTool::new();
        assert_eq!(tool.name(), "stt");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
        assert!(schema
            .get("required")
            .unwrap()
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("audio")));
    }

    #[tokio::test]
    async fn test_stt_tool_invalid_args() {
        let tool = SttTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid arguments"));
    }

    #[tokio::test]
    async fn test_stt_missing_file() {
        let tool = SttTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(serde_json::json!({"audio": "/nonexistent/file.mp3"}), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Audio file not found"));
    }

    #[tokio::test]
    async fn test_stt_no_api_key() {
        let tool = SttTool::new();
        let ctx = ToolContext::new("user", "conv");

        // Create a temp file so the file-exists check passes
        let dir = tempfile::tempdir().unwrap();
        let audio_file = dir.path().join("test.wav");
        tokio::fs::write(&audio_file, b"fake audio data")
            .await
            .unwrap();

        let result = tool
            .execute(serde_json::json!({"audio": audio_file.to_string_lossy()}), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .unwrap()
                .contains("needs a configured provider"),
            "with no provider the tool says so rather than reading an env var"
        );
    }
}
