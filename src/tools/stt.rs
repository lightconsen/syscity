//! STT Tool — Speech-to-Text
//!
//! tool for transcribing audio to text.
//! Uses OpenAI Whisper API as the backend.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

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
    /// Response format: text, srt, vtt, json, verbose_json (default: text)
    #[serde(default)]
    response_format: Option<String>,
    /// Sampling temperature (0.0 - 1.0)
    #[serde(default)]
    temperature: Option<f32>,
    /// Optional context prompt to guide the transcription
    #[serde(default)]
    prompt: Option<String>,
}

/// Map file extension to a MIME type for the multipart upload.
fn mime_for_ext(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "audio/mp4",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("webm") => "audio/webm",
        Some("ogg") => "audio/ogg",
        Some("flac") => "audio/flac",
        Some("aiff") | Some("aif") => "audio/aiff",
        Some("opus") => "audio/opus",
        _ => "application/octet-stream",
    }
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
                "response_format": {
                    "type": "string",
                    "description": "Output format: text, srt, vtt, json, verbose_json",
                    "default": "text",
                    "enum": ["text", "srt", "vtt", "json", "verbose_json"]
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

        // Try OpenAI Whisper API
        let api_key = context
            .environment()
            .get("OPENAI_API_KEY")
            .cloned()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok());
        if let Some(api_key) = api_key {
            let model = args.model.as_deref().unwrap_or("whisper-1");
            let response_format = args.response_format.as_deref().unwrap_or("text");

            // Read the audio file
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

            let file_name = audio_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "audio.bin".to_string());

            let mime = mime_for_ext(&audio_path);

            // Build multipart form
            let mut form = reqwest::multipart::Form::new()
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(audio_bytes)
                        .file_name(file_name)
                        .mime_str(mime)
                        .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new())),
                )
                .text("model", model.to_string())
                .text("response_format", response_format.to_string());

            if let Some(lang) = &args.language {
                form = form.text("language", lang.clone());
            }
            if let Some(temp) = args.temperature {
                form = form.text("temperature", temp.to_string());
            }
            if let Some(prompt_text) = &args.prompt {
                form = form.text("prompt", prompt_text.clone());
            }

            let client = reqwest::Client::new();
            let response = client
                .post("https://api.openai.com/v1/audio/transcriptions")
                .header("Authorization", format!("Bearer {}", api_key))
                .multipart(form)
                .timeout(std::time::Duration::from_secs(120))
                .send()
                .await;

            match response {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    info!("STT transcription completed via OpenAI Whisper");

                    return Ok(ToolExecutionResult {
                        success: true,
                        output: text.clone(),
                        error: None,
                        data: Some(serde_json::json!({
                            "text": text,
                            "model": model,
                            "provider": "openai_whisper",
                            "audio_file": audio_path.to_string_lossy(),
                        })),
                        execution_time: start.elapsed(),
                    });
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    warn!("OpenAI Whisper API error {}: {}", status, body);
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("OpenAI Whisper API returned {}: {}", status, body)),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }
                Err(e) => {
                    warn!("OpenAI Whisper request failed: {}", e);
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("OpenAI Whisper request failed: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }
            }
        }

        // No API key available
        Ok(ToolExecutionResult {
            success: false,
            output: String::new(),
            error: Some(
                "No STT provider available. Set OPENAI_API_KEY to use OpenAI Whisper API."
                    .to_string(),
            ),
            data: Some(serde_json::json!({
                "audio_file": audio_path.to_string_lossy(),
            })),
            execution_time: start.elapsed(),
        })
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
        assert_eq!(args.response_format, None);
        assert_eq!(args.temperature, None);
        assert_eq!(args.prompt, None);
    }

    #[test]
    fn test_stt_args_custom() {
        let args: SttArgs = serde_json::from_value(serde_json::json!({
            "audio": "/tmp/speech.wav",
            "model": "whisper-1",
            "language": "zh",
            "response_format": "json",
            "temperature": 0.5,
            "prompt": "technical conversation"
        }))
        .unwrap();
        assert_eq!(args.audio, "/tmp/speech.wav");
        assert_eq!(args.model, Some("whisper-1".to_string()));
        assert_eq!(args.language, Some("zh".to_string()));
        assert_eq!(args.response_format, Some("json".to_string()));
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
        assert!(result.error.unwrap().contains("No STT provider"));
    }

    #[test]
    fn test_mime_for_ext() {
        assert_eq!(mime_for_ext(Path::new("test.mp3")), "audio/mpeg");
        assert_eq!(mime_for_ext(Path::new("test.wav")), "audio/wav");
        assert_eq!(mime_for_ext(Path::new("test.ogg")), "audio/ogg");
        assert_eq!(mime_for_ext(Path::new("test.flac")), "audio/flac");
        assert_eq!(mime_for_ext(Path::new("test.m4a")), "audio/mp4");
        assert_eq!(mime_for_ext(Path::new("test.unknown")), "application/octet-stream");
    }
}
