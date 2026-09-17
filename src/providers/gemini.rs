//! Google Gemini provider implementation
//!
//! Uses the `x-goog-api-key` header for authentication and the
//! `generateContent` / `streamGenerateContent` endpoints on
//! `generativelanguage.googleapis.com`.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use super::{
    CompletionChunk, CompletionRequest, CompletionResponse, CompletionStream, Message, Provider,
    ProviderInstanceConfig, Role, ToolCall, Usage,
};
use crate::model_router::gateway_client::GatewayClient;
use crate::model_router::HttpGatewayClient;

/// Google Gemini provider
#[derive(Debug, Clone)]
pub struct GeminiProvider {
    /// Base URL (default: https://generativelanguage.googleapis.com/v1beta)
    base_url: String,
    /// Default model (default: gemini-2.0-flash)
    default_model: String,
    /// Per-provider context window override from config (0 = use model-based
    /// estimate)
    max_context: usize,
    /// Unified HTTP client with retry/backoff, auth, and rate limiting
    gateway_client: std::sync::Arc<HttpGatewayClient>,
}

impl GeminiProvider {
    /// Create a new Gemini provider from an API key string.
    pub fn new(api_key: impl Into<String>) -> crate::Result<Self> {
        Self::with_credential(crate::model_router::Credential::api_key(api_key))
    }

    /// Create with a full `Credential`.
    pub fn with_credential(credential: crate::model_router::Credential) -> crate::Result<Self> {
        let mut extra_headers = HeaderMap::new();
        extra_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        extra_headers.insert("User-Agent", HeaderValue::from_static("syscity/1.0"));
        extra_headers.insert("Accept", HeaderValue::from_static("application/json"));

        let gateway_client = std::sync::Arc::new(
            HttpGatewayClient::new(
                "https://generativelanguage.googleapis.com/v1beta",
                credential,
                Duration::from_secs(180),
            )?
            .with_api_key_header("x-goog-api-key")
            .with_extra_headers(extra_headers),
        );

        Ok(Self {
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            default_model: "gemini-2.0-flash".to_string(),
            max_context: 0,
            gateway_client,
        })
    }

    /// Set a custom base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the default model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Create from a fully-resolved `ProviderInstanceConfig`.
    pub fn from_config(config: &ProviderInstanceConfig) -> crate::Result<Self> {
        let credential =
            crate::model_router::Credential::api_key(config.api_key.clone().unwrap_or_default());
        let mut this = Self::with_credential(credential)?;
        this.base_url = config.base_url.clone();
        this.default_model = config.model.clone();
        this.max_context = config.max_context;
        Ok(this)
    }

    /// Convert internal messages to Gemini contents.
    fn to_gemini_contents(messages: &[Message]) -> Vec<GeminiContent> {
        messages
            .iter()
            .filter_map(|msg| {
                let role = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "model",
                    Role::System => {
                        // Gemini doesn't have a system role in the same way;
                        // system messages are handled via system_instruction field
                        return None;
                    }
                    Role::Tool => "user",
                };

                let mut parts = Vec::new();
                // Handle tool results wrapped as function_response
                if msg.role == Role::Tool {
                    parts.push(GeminiPart::FunctionResponse {
                        name: msg.name.clone().unwrap_or_default(),
                        response: serde_json::json!({
                            "content": msg.content,
                        }),
                    });
                } else if let Some(ref blocks) = msg.content_blocks {
                    for block in blocks {
                        match block {
                            super::ContentBlock::Text { text } => {
                                parts.push(GeminiPart::Text { text: text.clone() });
                            }
                            super::ContentBlock::Image { base64, mime_type } => {
                                parts.push(GeminiPart::InlineData {
                                    inline_data: GeminiInlineData {
                                        mime_type: mime_type.clone(),
                                        data: base64.clone(),
                                    },
                                });
                            }
                        }
                    }
                } else {
                    parts.push(GeminiPart::Text { text: msg.content.clone() });
                }

                // Include tool calls from assistant messages
                if let Some(ref calls) = msg.tool_calls {
                    for tc in calls {
                        let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or_else(|e| {
                                warn!("Failed to parse tool call arguments: {}", e);
                                serde_json::Value::Object(serde_json::Map::new())
                            });
                        parts.push(GeminiPart::FunctionCall {
                            name: tc.function.name.clone(),
                            args,
                        });
                    }
                }

                Some(GeminiContent { role: role.to_string(), parts })
            })
            .collect()
    }

    /// Extract system instruction from messages (Gemini handles it separately).
    fn extract_system_instruction(messages: &[Message]) -> Option<GeminiContent> {
        let system_text: Vec<&str> = messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
            .collect();
        if system_text.is_empty() {
            return None;
        }
        Some(GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart::Text { text: system_text.join("\n") }],
        })
    }

    /// Parse a Gemini generateContent response into internal format.
    fn parse_gemini_response(
        &self,
        resp: GeminiResponse,
        model: &str,
    ) -> crate::Result<CompletionResponse> {
        let candidate = resp.candidates.into_iter().next().ok_or_else(|| {
            crate::error::SyscityError::ExternalService {
                source: format!(
                    "No candidates returned: {}",
                    resp.prompt_feedback
                        .as_ref()
                        .map(|f| format!("{:?}", f.block_reason))
                        .unwrap_or_default()
                ),
                cause: None,
            }
        })?;
        let content = candidate.content.unwrap_or(GeminiContent {
            role: "model".to_string(),
            parts: vec![],
        });

        // Extract text and function calls from parts
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        for part in content.parts {
            match part {
                GeminiPart::Text { text } => text_parts.push(text),
                GeminiPart::FunctionCall { name, args } => {
                    tool_calls.push(ToolCall {
                        id: format!("fc_{}", name),
                        call_type: "function".to_string(),
                        function: super::FunctionCall {
                            name,
                            arguments: serde_json::to_string(&args)?,
                        },
                        index: None,
                        result: None,
                    });
                }
                _ => {}
            }
        }

        let finish_reason = candidate.finish_reason.map(|r| format!("{:?}", r));

        Ok(CompletionResponse {
            message: Message {
                role: Role::Assistant,
                content: text_parts.join("\n"),
                content_blocks: None,
                reasoning_content: None,
                name: None,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
                metadata: None,
            },
            usage: resp.usage_metadata.map(|u| Usage {
                prompt_tokens: u.prompt_token_count,
                completion_tokens: u.candidates_token_count,
                total_tokens: u.total_token_count,
                ..Default::default()
            }),
            model: model.to_string(),
            finish_reason,
        })
    }

    /// Build the request URL.
    fn url(&self, model: &str, streaming: bool) -> String {
        let endpoint = if streaming {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        format!("{}/models/{}:{}", self.base_url.trim_end_matches('/'), model, endpoint,)
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn max_context(&self) -> usize {
        if self.max_context > 0 {
            self.max_context
        } else {
            match self.default_model.as_str() {
                "gemini-2.0-flash" | "gemini-2.0-pro" | "gemini-1.5-pro" => 1_048_576,
                "gemini-1.5-flash" => 1_048_576,
                _ => 128_000,
            }
        }
    }

    fn stream_family(&self) -> super::stream_wrappers::ProviderStreamFamily {
        super::stream_wrappers::ProviderStreamFamily::GoogleThinking
    }

    #[instrument(skip(self, request))]
    async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse> {
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());
        info!("Gemini API request - model: {}", model);

        let system_instruction = Self::extract_system_instruction(&request.messages);
        let contents = Self::to_gemini_contents(&request.messages);

        // Build tool definitions
        let tools: Option<Vec<GeminiTool>> = request.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t| GeminiTool {
                    function_declarations: vec![GeminiFunctionDeclaration {
                        name: t.function.name,
                        description: t.function.description,
                        parameters: t.function.parameters,
                    }],
                })
                .collect()
        });

        let body = GeminiRequestBody {
            system_instruction,
            contents,
            tools,
            generation_config: Some(GeminiGenerationConfig {
                temperature: request.temperature,
                max_output_tokens: request.max_tokens,
                stop_sequences: request.stop,
            }),
        };

        let request_url = self.url(&model, false);

        let gemini_resp: GeminiResponse =
            self.gateway_client.post_json(&request_url, &body).await?;

        info!("Successfully received completion from Gemini");
        self.parse_gemini_response(gemini_resp, &model)
    }

    async fn stream(&self, request: CompletionRequest) -> crate::Result<CompletionStream> {
        debug!("Starting streaming completion from Gemini");

        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());

        let system_instruction = Self::extract_system_instruction(&request.messages);
        let contents = Self::to_gemini_contents(&request.messages);

        let tools: Option<Vec<GeminiTool>> = request.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t| GeminiTool {
                    function_declarations: vec![GeminiFunctionDeclaration {
                        name: t.function.name,
                        description: t.function.description,
                        parameters: t.function.parameters,
                    }],
                })
                .collect()
        });

        let body = GeminiRequestBody {
            system_instruction,
            contents,
            tools,
            generation_config: Some(GeminiGenerationConfig {
                temperature: request.temperature,
                max_output_tokens: request.max_tokens,
                stop_sequences: request.stop,
            }),
        };

        let request_url = self.url(&model, true);

        let response = self
            .gateway_client
            .post_json_streaming(&request_url, &body)
            .await?;

        let stream = response.bytes_stream();
        let gemini_stream = GeminiStream::new(stream);
        return Ok(Box::pin(gemini_stream));
    }

    async fn health_check(&self) -> crate::Result<bool> {
        let model = &self.default_model;
        let url = format!("{}/models/{}", self.base_url.trim_end_matches('/'), model);
        match self.gateway_client.get(&url).await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    async fn set_credential(
        &self,
        credential: crate::model_router::Credential,
    ) -> crate::Result<()> {
        self.gateway_client.set_credential(credential).await;
        Ok(())
    }
}

// ------------------------------------------------------------------
// Gemini API types
// ------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct GeminiRequestBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Debug)]
enum GeminiPart {
    Text {
        text: String,
    },
    InlineData {
        inline_data: GeminiInlineData,
    },
    FunctionCall {
        name: String,
        args: serde_json::Value,
    },
    FunctionResponse {
        name: String,
        response: serde_json::Value,
    },
}

// GeminiPart has custom Serde implementations below — do not use derive for it.

/// Structured representation of inline data for Gemini parts.
#[derive(Debug, Serialize, Deserialize)]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

impl Serialize for GeminiPart {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            GeminiPart::Text { text } => {
                let mut map = serde_json::Map::new();
                map.insert("text".to_string(), serde_json::json!(text));
                serde_json::Value::Object(map).serialize(serializer)
            }
            GeminiPart::InlineData { inline_data } => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "inlineData".to_string(),
                    serde_json::json!({
                        "mimeType": inline_data.mime_type,
                        "data": inline_data.data,
                    }),
                );
                serde_json::Value::Object(map).serialize(serializer)
            }
            GeminiPart::FunctionCall { name, args } => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "functionCall".to_string(),
                    serde_json::json!({
                        "name": name,
                        "args": args,
                    }),
                );
                serde_json::Value::Object(map).serialize(serializer)
            }
            GeminiPart::FunctionResponse { name, response } => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "functionResponse".to_string(),
                    serde_json::json!({
                        "name": name,
                        "response": response,
                    }),
                );
                serde_json::Value::Object(map).serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for GeminiPart {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let map = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected object for GeminiPart"))?;

        if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
            return Ok(GeminiPart::Text { text: text.to_string() });
        }
        if let Some(inline_data_val) = map.get("inlineData") {
            let mime_type = inline_data_val
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let data = inline_data_val
                .get("data")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Ok(GeminiPart::InlineData {
                inline_data: GeminiInlineData { mime_type, data },
            });
        }
        if let Some(fc) = map.get("functionCall") {
            let name = fc
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = fc
                .get("args")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));
            return Ok(GeminiPart::FunctionCall { name, args });
        }
        if let Some(fr) = map.get("functionResponse") {
            let name = fr
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let response = fr
                .get("response")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));
            return Ok(GeminiPart::FunctionResponse { name, response });
        }

        Err(serde::de::Error::custom(format!(
            "unknown GeminiPart variant: {}",
            serde_json::to_string(&value).unwrap_or_default()
        )))
    }
}

#[derive(Debug, Serialize)]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    prompt_feedback: Option<GeminiPromptFeedback>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiPromptFeedback {
    block_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<GeminiFinishReason>,
    index: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum GeminiFinishReason {
    Stop,
    MaxTokens,
    Safety,
    Recitation,
    Other,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiUsageMetadata {
    prompt_token_count: u32,
    candidates_token_count: u32,
    total_token_count: u32,
}

// Streaming types
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiStreamResponse {
    candidates: Option<Vec<GeminiStreamCandidate>>,
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GeminiStreamCandidate {
    content: Option<GeminiContent>,
    finish_reason: Option<GeminiFinishReason>,
    index: Option<u32>,
}

/// Gemini SSE stream parser (returns JSON objects separated by newlines).
struct GeminiStream {
    buffer: String,
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
}

impl GeminiStream {
    fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            buffer: String::new(),
            inner: Box::pin(stream),
        }
    }

    fn parse_line(&mut self, line: &str) -> Option<CompletionChunk> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("data:") {
            return None;
        }

        // Try to parse a Gemini streaming response
        if let Ok(response) = serde_json::from_str::<GeminiStreamResponse>(trimmed) {
            if let Some(candidates) = response.candidates {
                let mut all_content = Vec::new();
                let mut is_done = false;

                for candidate in candidates {
                    if let Some(ref content) = candidate.content {
                        for part in &content.parts {
                            if let GeminiPart::Text { text } = part {
                                all_content.push(text.clone());
                            }
                        }
                    }
                    if candidate.finish_reason.is_some() {
                        is_done = true;
                    }
                }

                let content = if all_content.is_empty() {
                    None
                } else {
                    Some(all_content.join(""))
                };

                return Some(CompletionChunk {
                    content,
                    reasoning_content: None,
                    tool_calls: None,
                    is_done,
                    error: None,
                    usage: response.usage_metadata.map(|u| Usage {
                        prompt_tokens: u.prompt_token_count,
                        completion_tokens: u.candidates_token_count,
                        total_tokens: u.total_token_count,
                        ..Default::default()
                    }),
                });
            }

            // Check for usage-only response (final chunk without candidates)
            if let Some(usage) = response.usage_metadata {
                return Some(CompletionChunk {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    is_done: true,
                    error: None,
                    usage: Some(Usage {
                        prompt_tokens: usage.prompt_token_count,
                        completion_tokens: usage.candidates_token_count,
                        total_tokens: usage.total_token_count,
                        ..Default::default()
                    }),
                });
            }
        }

        None
    }
}

impl Stream for GeminiStream {
    type Item = CompletionChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Process complete lines in the buffer first
            while let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].to_string();
                self.buffer = self.buffer[pos + 1..].to_string();
                if let Some(chunk) = self.parse_line(&line) {
                    return Poll::Ready(Some(chunk));
                }
            }

            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let chunk = String::from_utf8_lossy(&bytes);
                    self.buffer.push_str(&chunk);
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!("Gemini stream error: {}", e);
                    return Poll::Ready(Some(CompletionChunk {
                        content: None,
                        reasoning_content: None,
                        tool_calls: None,
                        is_done: true,
                        error: Some(e.to_string()),
                        usage: None,
                    }));
                }
                Poll::Ready(None) => {
                    if !self.buffer.is_empty() {
                        let line = self.buffer.clone();
                        self.buffer.clear();
                        if let Some(chunk) = self.parse_line(&line) {
                            return Poll::Ready(Some(chunk));
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gemini_provider_name() {
        let provider = GeminiProvider::new("test-key").unwrap();
        assert_eq!(provider.name(), "gemini");
    }

    #[test]
    fn test_gemini_provider_default_model() {
        let provider = GeminiProvider::new("test-key").unwrap();
        assert_eq!(provider.default_model(), "gemini-2.0-flash");
    }

    #[test]
    fn test_gemini_provider_with_model() {
        let provider = GeminiProvider::new("test-key")
            .unwrap()
            .with_model("gemini-1.5-pro");
        assert_eq!(provider.default_model(), "gemini-1.5-pro");
    }

    #[test]
    fn test_gemini_provider_supports_tools() {
        let provider = GeminiProvider::new("test-key").unwrap();
        assert!(provider.supports_tools());
    }

    #[test]
    fn test_gemini_provider_max_context() {
        let provider = GeminiProvider::new("test-key").unwrap();
        assert_eq!(provider.max_context(), 1_048_576);
    }

    #[test]
    fn test_gemini_provider_url() {
        let provider = GeminiProvider::new("test-key").unwrap();
        let url = provider.url("gemini-2.0-flash", false);
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent"
        );
    }

    #[test]
    fn test_gemini_provider_stream_url() {
        let provider = GeminiProvider::new("test-key").unwrap();
        let url = provider.url("gemini-2.0-flash", true);
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:streamGenerateContent"
        );
    }

    #[test]
    fn test_gemini_to_contents() {
        let messages = vec![Message::user("Hello"), Message::assistant("Hi there!")];
        let contents = GeminiProvider::to_gemini_contents(&messages);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[1].role, "model");
    }

    #[test]
    fn test_gemini_extract_system_instruction() {
        let messages = vec![
            Message::system("You are a helpful assistant"),
            Message::user("Hello"),
        ];
        let instruction = GeminiProvider::extract_system_instruction(&messages);
        assert!(instruction.is_some());
        let instruction = instruction.unwrap();
        assert_eq!(instruction.parts.len(), 1);
    }

    #[test]
    fn test_gemini_from_response() {
        let provider = GeminiProvider::new("test-key").unwrap();
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: Some(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![GeminiPart::Text { text: "Hello back".to_string() }],
                }),
                finish_reason: Some(GeminiFinishReason::Stop),
                index: Some(0),
            }],
            prompt_feedback: None,
            usage_metadata: None,
        };

        let result = provider
            .parse_gemini_response(resp, "gemini-2.0-flash")
            .unwrap();
        assert_eq!(result.message.content, "Hello back");
        assert_eq!(result.model, "gemini-2.0-flash");
        assert_eq!(result.message.role, Role::Assistant);
        assert!(result.message.tool_calls.is_none());
    }

    #[test]
    fn test_gemini_from_response_no_candidates() {
        let provider = GeminiProvider::new("test-key").unwrap();
        let resp = GeminiResponse {
            candidates: vec![],
            prompt_feedback: None,
            usage_metadata: None,
        };
        let result = provider.parse_gemini_response(resp, "gemini-2.0-flash");
        assert!(result.is_err());
    }

    #[test]
    fn test_gemini_part_deserialize_text() {
        let json = r#"{"text": "hello"}"#;
        let part: GeminiPart = serde_json::from_str(json).unwrap();
        match part {
            GeminiPart::Text { text } => assert_eq!(text, "hello"),
            _ => panic!("Expected Text variant"),
        }
    }

    #[tokio::test]
    async fn test_set_credential_updates_api_key() {
        let provider = GeminiProvider::new("first-key").unwrap();
        provider
            .set_credential(crate::model_router::Credential::api_key("rotated-key"))
            .await
            .unwrap();

        let cred = provider.gateway_client.credential.read().await;
        let (header_name, header_value) = provider.gateway_client.auth_for_credential(&cred);
        assert_eq!(header_name, "x-goog-api-key");
        assert_eq!(header_value, "rotated-key");
    }

    #[tokio::test]
    async fn test_set_credential_to_bearer_token() {
        let provider = GeminiProvider::new("first-key").unwrap();
        provider
            .set_credential(crate::model_router::Credential::bearer_token("oauth-token"))
            .await
            .unwrap();

        let cred = provider.gateway_client.credential.read().await;
        let (header_name, header_value) = provider.gateway_client.auth_for_credential(&cred);
        assert_eq!(header_name, "Authorization");
        assert_eq!(header_value, "Bearer oauth-token");
    }

    // ------------------------------------------------------------------
    // GeminiStream parse_line tests
    // ------------------------------------------------------------------

    struct GeminiEmptyStream;
    impl futures::Stream for GeminiEmptyStream {
        type Item = Result<bytes::Bytes, reqwest::Error>;
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    fn make_stream() -> GeminiStream {
        GeminiStream::new(GeminiEmptyStream)
    }

    #[test]
    fn test_parse_line_text_content() {
        let mut stream = make_stream();
        let line = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}]}"#;
        let chunk = stream.parse_line(line);
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert_eq!(chunk.content, Some("Hello".to_string()));
        assert!(!chunk.is_done);
    }

    #[test]
    fn test_parse_line_finish_reason() {
        let mut stream = make_stream();
        let line = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finish_reason":"STOP"}]}"#;
        let chunk = stream.parse_line(line);
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert_eq!(chunk.content, Some("done".to_string()));
        assert!(chunk.is_done);
    }

    #[test]
    fn test_parse_line_usage_only() {
        let mut stream = make_stream();
        let line = r#"{"usage_metadata":{"prompt_token_count":10,"candidates_token_count":5,"total_token_count":15}}"#;
        let chunk = stream.parse_line(line);
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert!(chunk.is_done);
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn test_parse_line_empty_returns_none() {
        let mut stream = make_stream();
        assert!(stream.parse_line("").is_none());
        assert!(stream.parse_line("   ").is_none());
    }

    #[test]
    fn test_parse_line_data_prefix_returns_none() {
        let mut stream = make_stream();
        assert!(stream.parse_line("data: some text").is_none());
    }

    #[test]
    fn test_parse_line_invalid_json_returns_none() {
        let mut stream = make_stream();
        assert!(stream.parse_line("not json at all").is_none());
    }

    #[test]
    fn test_parse_line_multiple_text_parts_joined() {
        let mut stream = make_stream();
        let line = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello "},{"text":"world"}]}}]}"#;
        let chunk = stream.parse_line(line);
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert_eq!(chunk.content, Some("Hello world".to_string()));
    }
}
