//! TTS Tool — Text-to-Speech
//!
//! tool for converting text to speech.
//! Supports OpenAI TTS API as the primary backend, with
//! fallback to macOS `say` command or festival/espeak.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::process_runner::ProcessRequest;
use crate::tools::sdk::ToolCapabilities;

/// Text-to-speech tool
pub struct TtsTool;

impl TtsTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TtsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct TtsArgs {
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
    #[serde(default)]
    output: Option<String>,
}

#[async_trait]
impl Tool for TtsTool {
    fn name(&self) -> &str {
        "tts"
    }

    fn description(&self) -> &str {
        "Convert text to speech audio. Use this tool when the user asks to read aloud, speak, \
         or convert text to speech/audio (文字转语音/朗读). Requires OPENAI_API_KEY or uses \
         local system TTS."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Text to speak"
                },
                "voice": {
                    "type": "string",
                    "description": "Voice name (OpenAI: alloy/echo/fable/onyx/nova/shimmer; macOS: system voice names)",
                    "default": "alloy"
                },
                "speed": {
                    "type": "number",
                    "description": "Speech speed multiplier (0.25 - 4.0)",
                    "default": 1.0
                },
                "output": {
                    "type": "string",
                    "description": "Output file path (default: auto-generated .mp3 or .aiff)"
                }
            },
            "required": ["text"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["media".to_string(), "audio".to_string()],
            ..Default::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        // Uses `say`/`espeak` subprocesses; no mobile equivalent yet (§4.4).
        !cfg!(mobile_os)
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: TtsArgs = match serde_json::from_value(args) {
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

        let output_path = if let Some(out) = args.output {
            std::path::PathBuf::from(out)
        } else {
            context
                .working_directory()
                .join(format!("tts_{}.mp3", uuid::Uuid::new_v4()))
        };

        // The configured provider first — the same one the agent uses, with its
        // credential and fallback chain — and only then the local speech binary.
        if let Some(provider) = context.media_provider().cloned() {
            let voice = args.voice.clone().unwrap_or_else(|| "alloy".to_string());
            let speed = args.speed.unwrap_or(1.0).clamp(0.25, 4.0);

            let mut request = crate::providers::media::MediaRequest::new(
                crate::providers::media::MediaKind::Speech,
                args.text.clone(),
            );
            request.voice = Some(voice.clone());
            request.speed = Some(f64::from(speed));

            match provider.generate_media(request).await {
                Ok(response) => match response.output {
                    crate::providers::media::MediaOutput::Bytes { data, .. } => {
                        if let Err(e) = tokio::fs::write(&output_path, &data).await {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Failed to save audio: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }

                        info!("TTS audio generated via the configured provider: {:?}", output_path);
                        return Ok(ToolExecutionResult {
                            success: true,
                            output: format!("TTS audio saved: {}", output_path.display()),
                            error: None,
                            data: Some(serde_json::json!({
                                "local_path": output_path.to_string_lossy(),
                                "voice": voice,
                                "speed": speed,
                                "provider": provider.name(),
                            })),
                            execution_time: start.elapsed(),
                        });
                    }
                    other => {
                        warn!("TTS provider answered with something other than audio: {:?}", other);
                    }
                },
                Err(e) => {
                    warn!("TTS provider failed, falling back to local speech: {}", e);
                }
            }
        }

        // Fallback: macOS say command
        #[cfg(target_os = "macos")]
        {
            let output_aiff = output_path.with_extension("aiff");
            let voice = args.voice.as_deref().unwrap_or("");
            let mut say_argv = vec!["say".to_string()];
            if !voice.is_empty() && voice != "alloy" {
                say_argv.push("-v".to_string());
                say_argv.push(voice.to_string());
            }
            say_argv.push("-o".to_string());
            say_argv.push(output_aiff.to_string_lossy().into_owned());
            say_argv.push(args.text.clone());

            let req = ProcessRequest {
                argv: say_argv,
                ..Default::default()
            };
            match crate::tools::process_runner::run(&req).await {
                Ok(output) if output.success() => {
                    info!("TTS audio generated via macOS say: {:?}", output_aiff);
                    return Ok(ToolExecutionResult {
                        success: true,
                        output: format!("TTS audio saved: {}", output_aiff.display()),
                        error: None,
                        data: Some(serde_json::json!({
                            "local_path": output_aiff.to_string_lossy(),
                            "provider": "macos_say",
                        })),
                        execution_time: start.elapsed(),
                    });
                }
                _ => {}
            }
        }

        // Fallback: espeak (Linux)
        {
            let output_wav = output_path.with_extension("wav");
            let req = ProcessRequest::argv(&[
                "espeak",
                "-w",
                output_wav.to_str().unwrap_or(""),
                &args.text,
            ]);

            match crate::tools::process_runner::run(&req).await {
                Ok(output) if output.success() => {
                    info!("TTS audio generated via espeak: {:?}", output_wav);
                    return Ok(ToolExecutionResult {
                        success: true,
                        output: format!("TTS audio saved: {}", output_wav.display()),
                        error: None,
                        data: Some(serde_json::json!({
                            "local_path": output_wav.to_string_lossy(),
                            "provider": "espeak",
                        })),
                        execution_time: start.elapsed(),
                    });
                }
                _ => {}
            }
        }

        // Final fallback: just return the text (no audio generated)
        Ok(ToolExecutionResult {
            success: false,
            output: String::new(),
            error: Some(
                "No TTS provider available. Set OPENAI_API_KEY, or install espeak (Linux) / use \
                 macOS."
                    .to_string(),
            ),
            data: Some(serde_json::json!({
                "text": args.text,
                "requested_voice": args.voice,
            })),
            execution_time: start.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tts_args_defaults() {
        let args: TtsArgs = serde_json::from_value(serde_json::json!({
            "text": "Hello world"
        }))
        .unwrap();
        assert_eq!(args.text, "Hello world");
        assert_eq!(args.voice, None);
        assert_eq!(args.speed, None);
        assert_eq!(args.output, None);
    }

    #[test]
    fn test_tts_args_custom() {
        let args: TtsArgs = serde_json::from_value(serde_json::json!({
            "text": "Hello",
            "voice": "nova",
            "speed": 1.5,
            "output": "/tmp/out.mp3"
        }))
        .unwrap();
        assert_eq!(args.voice, Some("nova".to_string()));
        assert_eq!(args.speed, Some(1.5));
        assert_eq!(args.output, Some("/tmp/out.mp3".to_string()));
    }

    #[test]
    fn test_tts_tool_name_and_schema() {
        let tool = TtsTool::new();
        assert_eq!(tool.name(), "tts");
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
        assert!(schema
            .get("required")
            .unwrap()
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("text")));
    }

    #[tokio::test]
    async fn test_tts_tool_execution_result() {
        let tool = TtsTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool
            .execute(serde_json::json!({ "text": "Hello" }), &ctx)
            .await
            .unwrap();

        // Clean up any generated TTS audio files from the working directory
        if let Ok(dir) = std::fs::read_dir(ctx.working_directory()) {
            for entry in dir.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("tts_") && (name.ends_with(".mp3") || name.ends_with(".aiff")) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        // On macOS, the `say` fallback may succeed even without API key.
        // On other platforms without espeak, this would fail.
        // We just verify the result is well-formed.
        if result.success {
            assert!(
                result.output.contains("TTS audio saved")
                    || result.output.contains("TTS audio saved")
            );
        } else {
            assert!(result.error.unwrap().contains("No TTS provider"));
        }
    }

    #[tokio::test]
    async fn test_tts_tool_invalid_args() {
        let tool = TtsTool::new();
        let ctx = ToolContext::new("user", "conv");
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid arguments"));
    }

    #[test]
    fn test_speed_clamping() {
        // Verify clamp behavior: values outside 0.25-4.0 should be clamped
        let low = 0.1_f32.clamp(0.25, 4.0);
        assert_eq!(low, 0.25);

        let high = 5.0_f32.clamp(0.25, 4.0);
        assert_eq!(high, 4.0);

        let mid = 1.5_f32.clamp(0.25, 4.0);
        assert_eq!(mid, 1.5);
    }
}
