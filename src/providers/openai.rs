//! OpenAI provider implementation for Syscity
//!
//! Supports GPT-4, GPT-3.5, and other OpenAI models.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use super::{
    stream_wrappers::ProviderStreamFamily, CompletionChunk, CompletionRequest, CompletionResponse,
    CompletionStream, FunctionDefinition, Message, Provider, ProviderInstanceConfig, Role,
    ToolCall, Usage,
};
use crate::model_router::gateway_client::GatewayClient;
use crate::model_router::HttpGatewayClient;

/// OpenAI API client
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    /// Base URL (default: https://api.openai.com/v1)
    base_url: String,
    /// Default model
    default_model: String,
    /// Unified HTTP client with retry/backoff, auth, and rate limiting
    gateway_client: std::sync::Arc<HttpGatewayClient>,
    /// Optional stream family override (for protocol-variant vendors like
    /// Moonshot/Minimax)
    stream_family_override: Option<ProviderStreamFamily>,
    /// Maximum context length (0 = use model-based estimate).
    max_context: usize,
    /// Cached SYSCITY_API_PATH override, read once during construction.
    api_path_override: Option<String>,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider from an API key string
    /// (backward-compatible).
    pub fn new(api_key: impl Into<String>) -> crate::Result<Self> {
        Self::with_credential(crate::model_router::Credential::api_key(api_key))
    }

    /// Create with a custom base URL from an API key string
    /// (backward-compatible).
    pub fn with_base_url(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> crate::Result<Self> {
        let mut this = Self::with_credential(crate::model_router::Credential::api_key(api_key))?;
        this.base_url = base_url.into();
        Ok(this)
    }

    /// Create with a full `Credential` (supports OAuth2, Bearer token, API
    /// key).
    pub fn with_credential(credential: crate::model_router::Credential) -> crate::Result<Self> {
        let mut extra_headers = HeaderMap::new();
        extra_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Mimic curl's User-Agent to avoid API blocks
        extra_headers.insert("User-Agent", HeaderValue::from_static("curl/8.7.1"));
        extra_headers.insert("Accept", HeaderValue::from_static("application/json"));

        let gateway_client = std::sync::Arc::new(
            HttpGatewayClient::new(
                "https://api.openai.com/v1",
                credential,
                Duration::from_secs(180),
            )?
            .with_extra_headers(extra_headers),
        );

        Ok(Self {
            base_url: "https://api.openai.com/v1".to_string(),
            default_model: "gpt-4o-mini".to_string(),
            gateway_client,
            stream_family_override: None,
            max_context: 0,
            api_path_override: std::env::var("SYSCITY_API_PATH").ok(),
        })
    }

    /// Create from a fully-resolved `ProviderInstanceConfig`.
    ///
    /// This is the primary constructor used by the resolver; it sets all fields
    /// including protocol-variant-specific stream families.
    pub fn from_config(config: &ProviderInstanceConfig) -> crate::Result<Self> {
        let credential =
            crate::model_router::Credential::api_key(config.api_key.clone().unwrap_or_default());
        let mut this = Self::with_credential(credential)?;
        this.base_url = config.base_url.clone();
        this.default_model = config.model.clone();
        this.max_context = config.max_context;
        this.stream_family_override = Some(config.stream_family);
        Ok(this)
    }

    /// Set the default model
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Build the request URL
    fn url(&self, path: &str) -> String {
        if let Some(ref api_path) = self.api_path_override {
            format!("{}/{}", self.base_url.trim_end_matches('/'), api_path.trim_start_matches('/'))
        } else {
            format!("{}{}", self.base_url.trim_end_matches('/'), path)
        }
    }

    /// Get the base URL
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Convert internal message to OpenAI format
    fn to_openai_message(msg: &Message) -> OpenAiMessage {
        let content = if let Some(ref blocks) = msg.content_blocks {
            let parts: Vec<serde_json::Value> = blocks
                .iter()
                .map(|b| match b {
                    super::ContentBlock::Text { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    super::ContentBlock::Image { base64, mime_type } => {
                        serde_json::json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", mime_type, base64)
                            }
                        })
                    }
                })
                .collect();
            Some(serde_json::Value::Array(parts))
        } else if msg.content.is_empty() && msg.tool_calls.is_some() {
            // OpenAI API: content must be null/absent when tool_calls is present
            // DeepSeek rejects requests with content: "" alongside tool_calls
            None
        } else {
            Some(serde_json::Value::String(msg.content.clone()))
        };

        OpenAiMessage {
            role: match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            }
            .to_string(),
            content,
            reasoning_content: msg.reasoning_content.clone(),
            name: msg.name.clone(),
            tool_calls: msg.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|tc| OpenAiToolCall {
                        id: tc.id.clone(),
                        call_type: tc.call_type.clone(),
                        function: OpenAiFunctionCall {
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        },
                    })
                    .collect()
            }),
            tool_call_id: msg.tool_call_id.clone(),
        }
    }

    /// Convert OpenAI response to internal format
    #[allow(clippy::wrong_self_convention)]
    fn from_openai_response(&self, resp: OpenAiResponse) -> crate::Result<CompletionResponse> {
        let choice = resp.choices.into_iter().next().ok_or_else(|| {
            crate::error::SyscityError::ExternalService {
                source: "No completion choices returned".to_string(),
                cause: None,
            }
        })?;

        let content = match choice.message.content {
            Some(serde_json::Value::String(s)) => s,
            Some(serde_json::Value::Array(parts)) => {
                // Extract text from multimodal content parts
                parts
                    .iter()
                    .filter_map(|p| {
                        p.get("text")
                            .and_then(|t| t.as_str())
                            .map(|text| text.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => String::new(),
        };

        let message = Message {
            role: match choice.message.role.as_str() {
                "system" => Role::System,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                _ => Role::User,
            },
            content,
            content_blocks: None,
            reasoning_content: choice.message.reasoning_content,
            name: choice.message.name,
            tool_calls: choice.message.tool_calls.map(|calls| {
                calls
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: tc.id,
                        call_type: tc.call_type,
                        function: super::FunctionCall {
                            name: tc.function.name,
                            arguments: tc.function.arguments,
                        },
                        index: None,
                        result: None,
                    })
                    .collect()
            }),
            tool_call_id: choice.message.tool_call_id,
            metadata: None,
        };

        let x_credits_used = resp.x_credits_used;
        let x_credit_balance = resp.x_credit_balance;
        Ok(CompletionResponse {
            message,
            usage: resp
                .usage
                .map(|u| u.to_usage_with_credits(x_credits_used, x_credit_balance)),
            model: resp.model,
            finish_reason: choice.finish_reason,
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn max_context(&self) -> usize {
        if self.max_context > 0 {
            return self.max_context;
        }
        match self.default_model.as_str() {
            "gpt-4o" | "gpt-4-turbo" => 128_000,
            "gpt-4" => 8_192,
            "gpt-3.5-turbo" => 16_385,
            _ => 4_096,
        }
    }

    fn stream_family(&self) -> ProviderStreamFamily {
        if let Some(family) = self.stream_family_override {
            return family;
        }
        if self.default_model.starts_with("o1") || self.default_model.starts_with("o3") {
            ProviderStreamFamily::OpenAiReasoning
        } else {
            ProviderStreamFamily::OpenAi
        }
    }

    #[instrument(skip(self, request))]
    async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse> {
        self.gateway_client.refresh_credential_if_needed().await?;

        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());
        info!("OpenAI API request - model: {}, base_url: {}", model, self.base_url);

        let tools: Option<Vec<OpenAiTool>> = request.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t| OpenAiTool {
                    tool_type: "function".to_string(),
                    function: t.function,
                })
                .collect()
        });

        let body = OpenAiRequest {
            model: model.clone(),
            messages: request
                .messages
                .iter()
                .map(Self::to_openai_message)
                .collect(),
            tools,
            temperature: request.temperature.unwrap_or(0.7),
            max_tokens: request.max_tokens,
            stream: Some(false),
            stream_options: None,
            stop: request.stop,
        };

        // Merge provider-specific extra parameters
        let mut body_value = serde_json::to_value(&body)?;
        crate::providers::merge_extra(&mut body_value, request.extra);

        // Debug: print the actual request body
        let body_json = serde_json::to_string(&body_value).unwrap_or_default();
        debug!("OpenAI API request body: {}", body_json);

        let request_url = self.url("/chat/completions");
        debug!("OpenAI API full URL: {}", request_url);

        let openai_resp: OpenAiResponse = self
            .gateway_client
            .post_json(&request_url, &body_value)
            .await?;

        info!("Successfully received completion from OpenAI");
        self.from_openai_response(openai_resp)
    }

    async fn stream(&self, request: CompletionRequest) -> crate::Result<CompletionStream> {
        self.gateway_client.refresh_credential_if_needed().await?;

        debug!("Starting streaming completion from OpenAI");

        // Keep a copy for the non-streaming fallback replay (cloud relays
        // that reject `stream: true` with 501).
        #[cfg(feature = "cloud")]
        let fallback_request = request.clone();

        let model = request.model.unwrap_or_else(|| self.default_model.clone());

        let tools: Option<Vec<OpenAiTool>> = request.tools.map(|tools| {
            tools
                .into_iter()
                .map(|t| OpenAiTool {
                    tool_type: "function".to_string(),
                    function: t.function,
                })
                .collect()
        });

        let body = OpenAiRequest {
            model,
            messages: request
                .messages
                .iter()
                .map(Self::to_openai_message)
                .collect(),
            tools,
            temperature: request.temperature.unwrap_or(0.7),
            max_tokens: request.max_tokens,
            stream: Some(true),
            stream_options: Some(serde_json::json!({"include_usage": true})),
            stop: request.stop,
        };

        // Merge provider-specific extra parameters
        let mut body_value = serde_json::to_value(&body)?;
        crate::providers::merge_extra(&mut body_value, request.extra);

        // Debug: print the actual request body
        // let body_json = serde_json::to_string(&body_value).unwrap_or_default();
        // info!("OpenAI API streaming request body: {}", body_json);

        let request_url = self.url("/chat/completions");

        let response = match self
            .gateway_client
            .post_json_streaming(&request_url, &body_value)
            .await
        {
            Ok(resp) => resp,
            // Cloud relays may not implement streaming (501). Replay the
            // request non-streaming and surface the single response as a
            // one-chunk stream so chat works on such providers.
            #[cfg(feature = "cloud")]
            Err(e) if is_streaming_unsupported(&e) => {
                warn!("Provider rejected streaming ({}); replaying non-streaming", e);
                let resp = self.complete(fallback_request).await?;
                return Ok(Box::pin(futures::stream::once(async move {
                    CompletionChunk {
                        content: Some(resp.message.content),
                        reasoning_content: resp.message.reasoning_content,
                        tool_calls: resp.message.tool_calls,
                        is_done: true,
                        usage: resp.usage,
                    }
                })));
            }
            Err(e) => return Err(e),
        };

        let stream = response.bytes_stream();
        let openai_stream = OpenAiStream::new(stream);
        return Ok(Box::pin(openai_stream));
    }

    async fn health_check(&self) -> crate::Result<bool> {
        match self.gateway_client.get(&self.url("/models")).await {
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

/// Whether the error indicates the provider does not support streaming
/// (HTTP 501 from relays that only implement non-streaming completion).
#[cfg(feature = "cloud")]
fn is_streaming_unsupported(e: &crate::error::SyscityError) -> bool {
    match e {
        crate::error::SyscityError::ExternalService { source, .. } => {
            source.contains("HTTP 501") || source.contains("streaming not supported")
        }
        _ => false,
    }
}

// OpenAI API types

#[derive(Debug, Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool>>,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiMessage {
    role: String,
    /// Either a plain text string or an array of content parts for multimodal.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    /// Reasoning / thinking content returned by some models (e.g. Qwen)
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: FunctionDefinition,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAiFunctionCall,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiResponse {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
    /// Cloud relay metering extension (top-level on the response body).
    #[serde(default)]
    x_credits_used: Option<i64>,
    #[serde(default)]
    x_credit_balance: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiChoice {
    index: u32,
    message: OpenAiMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    /// OpenAI-style nested cache details
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// DeepSeek-style flat cache fields
    prompt_cache_hit_tokens: Option<u32>,
    #[allow(dead_code)]
    prompt_cache_miss_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
}

impl OpenAiUsage {
    /// Tokens served from the provider-side prompt cache.
    fn cache_read_tokens(&self) -> u32 {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
            .unwrap_or(0)
    }

    /// Map to `Usage`, carrying the cloud relay's top-level credit fields
    /// (`x_credits_used` / `x_credit_balance` sit on the response body, not
    /// inside `usage`).
    fn to_usage_with_credits(&self, used: Option<i64>, balance: Option<i64>) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cache_read_tokens: self.cache_read_tokens(),
            cache_creation_tokens: 0,
            x_credits_used: used,
            x_credit_balance: balance,
        }
    }
}

// SSE Streaming types

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiStreamResponse {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<OpenAiStreamChoice>,
    /// Present in the final chunk when `stream_options.include_usage` is set
    usage: Option<OpenAiUsage>,
    /// Cloud relay metering extension (top-level on the final chunk body).
    #[serde(default)]
    x_credits_used: Option<i64>,
    #[serde(default)]
    x_credit_balance: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAiStreamChoice {
    index: u32,
    delta: OpenAiDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct OpenAiDelta {
    role: Option<String>,
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAiStreamToolCall>>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct OpenAiStreamToolCall {
    index: u32,
    id: Option<String>,
    #[serde(rename = "type")]
    call_type: Option<String>,
    function: Option<OpenAiStreamFunctionCall>,
}

#[derive(Debug, Deserialize, Clone)]
struct OpenAiStreamFunctionCall {
    name: Option<String>,
    arguments: Option<String>,
}

/// OpenAI SSE stream parser
struct OpenAiStream {
    buffer: String,
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
}

impl OpenAiStream {
    fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            buffer: String::new(),
            inner: Box::pin(stream),
        }
    }

    fn parse_sse_line(&self, line: &str) -> Option<CompletionChunk> {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with(':') {
            return None;
        }

        // Parse data lines
        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                return Some(CompletionChunk {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    is_done: true,
                    usage: None,
                });
            }

            // Try to parse the JSON
            if let Ok(response) = serde_json::from_str::<OpenAiStreamResponse>(data) {
                if let Some(choice) = response.choices.first() {
                    let content = choice.delta.content.clone();
                    let reasoning_content = choice.delta.reasoning_content.clone();

                    // Convert tool calls — preserve partial deltas (streaming chunks may
                    // have only index + arguments without id/call_type/name)
                    let tool_calls = choice.delta.tool_calls.as_ref().map(|calls| {
                        calls
                            .iter()
                            .map(|tc| ToolCall {
                                id: tc.id.clone().unwrap_or_default(),
                                call_type: tc.call_type.clone().unwrap_or_default(),
                                function: super::FunctionCall {
                                    name: tc
                                        .function
                                        .as_ref()
                                        .and_then(|f| f.name.clone())
                                        .unwrap_or_default(),
                                    arguments: tc
                                        .function
                                        .as_ref()
                                        .and_then(|f| f.arguments.clone())
                                        .unwrap_or_default(),
                                },
                                result: None,
                                index: Some(tc.index),
                            })
                            .collect()
                    });

                    let is_done = choice.finish_reason.is_some();

                    return Some(CompletionChunk {
                        content,
                        reasoning_content,
                        tool_calls,
                        is_done,
                        usage: response.usage.as_ref().map(|u| {
                            u.to_usage_with_credits(
                                response.x_credits_used,
                                response.x_credit_balance,
                            )
                        }),
                    });
                }
                // Usage-only final chunk (choices empty when include_usage set)
                if let Some(usage) = response.usage.as_ref() {
                    return Some(CompletionChunk {
                        content: None,
                        reasoning_content: None,
                        tool_calls: None,
                        is_done: true,
                        usage: Some(usage.to_usage_with_credits(
                            response.x_credits_used,
                            response.x_credit_balance,
                        )),
                    });
                }
            }
        }

        None
    }
}

impl Stream for OpenAiStream {
    type Item = CompletionChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // First, try to process any complete lines already in the buffer.
            // This prevents data loss when multiple SSE events arrive in a
            // single chunk and we returned early after processing the first one.
            while let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].to_string();
                self.buffer = self.buffer[pos + 1..].to_string();
                if let Some(chunk) = self.parse_sse_line(&line) {
                    return Poll::Ready(Some(chunk));
                }
            }

            // No complete lines in buffer — poll for more data.
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    if let Ok(chunk) = std::str::from_utf8(&bytes) {
                        self.buffer.push_str(chunk);
                    }
                    // Continue loop to process the newly appended data.
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!("Stream error: {}", e);
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => {
                    // Inner stream is done. Process any remaining text
                    // that does not end with a newline.
                    if !self.buffer.is_empty() {
                        let line = self.buffer.clone();
                        self.buffer.clear();
                        if let Some(chunk) = self.parse_sse_line(&line) {
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
    fn test_openai_message_conversion() {
        let msg = Message::user("Hello");
        let openai = OpenAiProvider::to_openai_message(&msg);
        assert_eq!(openai.role, "user");
        assert_eq!(openai.content, Some(serde_json::json!("Hello")));
    }

    #[test]
    fn test_openai_message_conversion_assistant() {
        let msg = Message::assistant("Hi there!");
        let openai = OpenAiProvider::to_openai_message(&msg);
        assert_eq!(openai.role, "assistant");
    }

    #[test]
    fn test_max_context() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        assert!(provider.max_context() > 0);
    }

    #[test]
    fn test_url_building() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        assert_eq!(provider.url("/chat/completions"), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn test_openai_provider_with_model() {
        let provider = OpenAiProvider::new("test-key")
            .unwrap()
            .with_model("gpt-4o");
        assert_eq!(provider.default_model(), "gpt-4o");
    }

    #[test]
    fn test_openai_provider_base_url() {
        let provider =
            OpenAiProvider::with_base_url("test-key", "https://proxy.example.com/v1").unwrap();
        assert_eq!(provider.base_url(), "https://proxy.example.com/v1");
    }

    #[test]
    fn test_openai_provider_max_context_gpt4o() {
        let provider = OpenAiProvider::new("test-key")
            .unwrap()
            .with_model("gpt-4o");
        assert_eq!(provider.max_context(), 128_000);
    }

    #[test]
    fn test_openai_provider_max_context_gpt4_turbo() {
        let provider = OpenAiProvider::new("test-key")
            .unwrap()
            .with_model("gpt-4-turbo");
        assert_eq!(provider.max_context(), 128_000);
    }

    #[test]
    fn test_openai_provider_max_context_gpt4() {
        let provider = OpenAiProvider::new("test-key").unwrap().with_model("gpt-4");
        assert_eq!(provider.max_context(), 8_192);
    }

    #[test]
    fn test_openai_provider_max_context_gpt35() {
        let provider = OpenAiProvider::new("test-key")
            .unwrap()
            .with_model("gpt-3.5-turbo");
        assert_eq!(provider.max_context(), 16_385);
    }

    #[test]
    fn test_openai_provider_max_context_unknown() {
        let provider = OpenAiProvider::new("test-key")
            .unwrap()
            .with_model("custom-model");
        assert_eq!(provider.max_context(), 4_096);
    }

    #[test]
    fn test_openai_provider_name() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_openai_provider_supports_tools() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        assert!(provider.supports_tools());
    }

    #[test]
    fn test_openai_provider_url_with_custom_path() {
        // SAFETY: Setting/removing an environment variable in a single-threaded
        // test context is safe. The unsafe block is only to satisfy the deny lint.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("SYSCITY_API_PATH", "custom/path")
        };
        let provider = OpenAiProvider::new("test-key").unwrap();
        let url = provider.url("/chat/completions");
        assert_eq!(url, "https://api.openai.com/v1/custom/path");
        // SAFETY: See comment above.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("SYSCITY_API_PATH")
        };
    }

    #[test]
    fn test_to_openai_message_system_role() {
        let msg = Message {
            role: Role::System,
            content: "You are helpful".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            metadata: None,
        };
        let openai = OpenAiProvider::to_openai_message(&msg);
        assert_eq!(openai.role, "system");
        assert_eq!(openai.content, Some(serde_json::json!("You are helpful")));
    }

    #[test]
    fn test_to_openai_message_tool_role() {
        let msg = Message {
            role: Role::Tool,
            content: "result".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: Some("call_123".to_string()),
            metadata: None,
        };
        let openai = OpenAiProvider::to_openai_message(&msg);
        assert_eq!(openai.role, "tool");
        assert_eq!(openai.content, Some(serde_json::json!("result")));
        assert_eq!(openai.tool_call_id, Some("call_123".to_string()));
    }

    #[test]
    fn test_to_openai_message_with_name() {
        let msg = Message {
            role: Role::User,
            content: "Hello".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: Some("alice".to_string()),
            tool_calls: None,
            tool_call_id: None,
            metadata: None,
        };
        let openai = OpenAiProvider::to_openai_message(&msg);
        assert_eq!(openai.name, Some("alice".to_string()));
    }

    #[test]
    fn test_to_openai_message_with_tool_calls() {
        let msg = Message {
            role: Role::Assistant,
            content: "".to_string(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: crate::providers::FunctionCall {
                    name: "test_tool".to_string(),
                    arguments: "{\"x\": 1}".to_string(),
                },
                index: None,
                result: None,
            }]),
            tool_call_id: None,
            metadata: None,
        };
        let openai = OpenAiProvider::to_openai_message(&msg);
        assert_eq!(openai.role, "assistant");
        let calls = openai.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].call_type, "function");
        assert_eq!(calls[0].function.name, "test_tool");
        assert_eq!(calls[0].function.arguments, "{\"x\": 1}");
    }

    #[test]
    fn test_to_openai_message_with_image() {
        let msg = Message::user("").with_content_blocks(vec![
            crate::providers::ContentBlock::text("Describe this"),
            crate::providers::ContentBlock::image_base64("abc123", "image/png"),
        ]);
        let openai = OpenAiProvider::to_openai_message(&msg);
        assert_eq!(openai.role, "user");
        let content = openai.content.unwrap();
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "Describe this");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,abc123");
    }

    #[test]
    fn test_from_openai_response_user_role() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        let resp = OpenAiResponse {
            id: "resp_1".to_string(),
            object: "chat.completion".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::json!("Hello")),
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: None,
            }],
            usage: None,
            x_credits_used: None,
            x_credit_balance: None,
        };
        let result = provider.from_openai_response(resp).unwrap();
        assert_eq!(result.message.role, Role::User);
        assert_eq!(result.message.content, "Hello");
        assert_eq!(result.model, "gpt-4");
        assert!(result.usage.is_none());
    }

    #[test]
    fn test_from_openai_response_tool_role() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        let resp = OpenAiResponse {
            id: "resp_1".to_string(),
            object: "chat.completion".to_string(),
            created: 1,
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "tool".to_string(),
                    content: Some(serde_json::json!("result")),
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
            x_credits_used: None,
            x_credit_balance: None,
        };
        let result = provider.from_openai_response(resp).unwrap();
        assert_eq!(result.message.role, Role::Tool);
        assert_eq!(result.message.content, "result");
        assert_eq!(result.message.tool_call_id, Some("call_1".to_string()));
        assert_eq!(result.finish_reason, Some("stop".to_string()));
    }

    #[test]
    fn test_from_openai_response_with_tool_calls() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        let resp = OpenAiResponse {
            id: "resp_1".to_string(),
            object: "chat.completion".to_string(),
            created: 1,
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::json!("")),
                    reasoning_content: None,
                    name: None,
                    tool_calls: Some(vec![OpenAiToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: OpenAiFunctionCall {
                            name: "test".to_string(),
                            arguments: "{}".to_string(),
                        },
                    }]),
                    tool_call_id: None,
                },
                finish_reason: None,
            }],
            usage: None,
            x_credits_used: None,
            x_credit_balance: None,
        };
        let result = provider.from_openai_response(resp).unwrap();
        assert!(result.message.tool_calls.is_some());
        let calls = result.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "test");
    }

    #[test]
    fn test_from_openai_response_with_usage() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        let resp = OpenAiResponse {
            id: "resp_1".to_string(),
            object: "chat.completion".to_string(),
            created: 1,
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::json!("Hi")),
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: None,
            }],
            usage: Some(OpenAiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
            x_credits_used: Some(3),
            x_credit_balance: Some(1234),
        };
        let result = provider.from_openai_response(resp).unwrap();
        let usage = result.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
        // Cloud relay metering extension is carried onto `Usage`.
        assert_eq!(usage.x_credits_used, Some(3));
        assert_eq!(usage.x_credit_balance, Some(1234));
    }

    #[test]
    fn test_openai_response_parses_credit_extension() {
        // Cloud relay metering fields sit at the top level of the response
        // body; they must be captured (and omitted when absent).
        let json = r#"{"id":"c1","object":"chat.completion","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3},"x_credits_used":2,"x_credit_balance":-3}"#;
        let resp: OpenAiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.x_credits_used, Some(2));
        assert_eq!(resp.x_credit_balance, Some(-3));
        let usage = resp
            .usage
            .unwrap()
            .to_usage_with_credits(resp.x_credits_used, resp.x_credit_balance);
        assert_eq!(usage.x_credits_used, Some(2));
        assert_eq!(usage.x_credit_balance, Some(-3));

        // Without the extension the fields stay `None` and do not serialize.
        let plain: OpenAiResponse = serde_json::from_str(
            r#"{"id":"c1","object":"chat.completion","created":1,"model":"m","choices":[]}"#,
        )
        .unwrap();
        assert_eq!(plain.x_credits_used, None);
        let usage = crate::providers::Usage::default();
        let ser = serde_json::to_string(&usage).unwrap();
        assert!(!ser.contains("x_credits_used"));
    }

    #[test]
    fn test_from_openai_response_no_choices() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        let resp = OpenAiResponse {
            id: "resp_1".to_string(),
            object: "chat.completion".to_string(),
            created: 1,
            model: "gpt-4".to_string(),
            choices: vec![],
            usage: None,
            x_credits_used: None,
            x_credit_balance: None,
        };
        let result = provider.from_openai_response(resp);
        assert!(result.is_err());
    }

    #[test]
    fn test_from_openai_response_empty_content() {
        let provider = OpenAiProvider::new("test-key").unwrap();
        let resp = OpenAiResponse {
            id: "resp_1".to_string(),
            object: "chat.completion".to_string(),
            created: 1,
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: None,
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: None,
            }],
            usage: None,
            x_credits_used: None,
            x_credit_balance: None,
        };
        let result = provider.from_openai_response(resp).unwrap();
        assert_eq!(result.message.content, "");
    }

    // Dummy stream for constructing OpenAiStream in tests
    struct EmptyStream;
    impl Stream for EmptyStream {
        type Item = Result<bytes::Bytes, reqwest::Error>;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(None)
        }
    }

    #[test]
    fn test_openai_stream_parse_done() {
        let stream = OpenAiStream::new(EmptyStream);
        let chunk = stream.parse_sse_line("data: [DONE]");
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert!(chunk.is_done);
        assert!(chunk.content.is_none());
        assert!(chunk.tool_calls.is_none());
    }

    #[test]
    fn test_openai_stream_parse_empty_line() {
        let stream = OpenAiStream::new(EmptyStream);
        assert!(stream.parse_sse_line("").is_none());
        assert!(stream.parse_sse_line("   ").is_none());
    }

    #[test]
    fn test_openai_stream_parse_comment() {
        let stream = OpenAiStream::new(EmptyStream);
        assert!(stream.parse_sse_line(": comment").is_none());
        assert!(stream.parse_sse_line(":ok").is_none());
    }

    #[test]
    fn test_openai_stream_parse_invalid_json() {
        let stream = OpenAiStream::new(EmptyStream);
        assert!(stream.parse_sse_line("data: not-json").is_none());
        assert!(stream.parse_sse_line("data: {}").is_none());
    }

    #[test]
    fn test_openai_stream_parse_valid_json() {
        let stream = OpenAiStream::new(EmptyStream);
        let json = r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk = stream.parse_sse_line(&format!("data: {}", json));
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert_eq!(chunk.content, Some("Hello".to_string()));
        assert!(!chunk.is_done);
        assert!(chunk.tool_calls.is_none());
    }

    #[test]
    fn test_openai_stream_parse_with_tool_calls() {
        let stream = OpenAiStream::new(EmptyStream);
        let json = r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"gpt-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"test","arguments":"{}"}}]},"finish_reason":null}]}"#;
        let chunk = stream.parse_sse_line(&format!("data: {}", json));
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert!(chunk.tool_calls.is_some());
        let calls = chunk.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].call_type, "function");
        assert_eq!(calls[0].function.name, "test");
        assert_eq!(calls[0].function.arguments, "{}");
    }

    #[test]
    fn test_openai_stream_parse_finish_reason() {
        let stream = OpenAiStream::new(EmptyStream);
        let json = r#"{"id":"1","object":"chat.completion.chunk","created":1,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let chunk = stream.parse_sse_line(&format!("data: {}", json));
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert!(chunk.is_done);
    }

    #[test]
    fn test_openai_request_serialization() {
        let req = OpenAiRequest {
            model: "gpt-4".to_string(),
            messages: vec![OpenAiMessage {
                role: "user".to_string(),
                content: Some(serde_json::json!("Hello")),
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: 0.7,
            max_tokens: Some(100),
            stream: Some(false),
            stream_options: None,
            stop: Some(vec!["STOP".to_string()]),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"model\":\"gpt-4\""));
        assert!(json.contains("\"temperature\":0.7"));
        assert!(json.contains("\"max_tokens\":100"));
        assert!(json.contains("\"stream\":false"));
        assert!(json.contains("\"stop\""));
    }

    #[test]
    fn test_openai_usage_openai_cache_tokens() {
        let json = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_tokens_details": {"cached_tokens": 80}
        });
        let usage: OpenAiUsage = serde_json::from_value(json).unwrap();
        assert_eq!(usage.cache_read_tokens(), 80);
        assert_eq!(usage.to_usage_with_credits(None, None).cache_read_tokens, 80);
    }

    #[test]
    fn test_openai_usage_deepseek_cache_tokens() {
        let json = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_cache_hit_tokens": 64,
            "prompt_cache_miss_tokens": 36
        });
        let usage: OpenAiUsage = serde_json::from_value(json).unwrap();
        assert_eq!(usage.cache_read_tokens(), 64);
    }

    #[test]
    fn test_openai_usage_no_cache_defaults_zero() {
        let json = serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        });
        let usage: OpenAiUsage = serde_json::from_value(json).unwrap();
        assert_eq!(usage.cache_read_tokens(), 0);
        assert_eq!(
            usage
                .to_usage_with_credits(None, None)
                .cache_creation_tokens,
            0
        );
    }

    #[test]
    fn test_parse_usage_only_final_chunk() {
        // When stream_options.include_usage is set, OpenAI/DeepSeek send a
        // final chunk with empty choices and populated usage.
        let data = r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":50,"completion_tokens":10,"total_tokens":60,"prompt_tokens_details":{"cached_tokens":30}}}"#;
        let response: OpenAiStreamResponse = serde_json::from_str(data).unwrap();
        assert!(response.choices.is_empty());
        let usage = response.usage.unwrap();
        assert_eq!(
            usage
                .to_usage_with_credits(response.x_credits_used, response.x_credit_balance)
                .cache_read_tokens,
            30
        );
    }

    #[tokio::test]
    async fn test_set_credential_updates_auth_header() {
        let provider = OpenAiProvider::new("first-key").unwrap();
        provider
            .set_credential(crate::model_router::Credential::api_key("rotated-key"))
            .await
            .unwrap();

        let cred = provider.gateway_client.credential.read().await;
        let (header_name, header_value) = provider.gateway_client.auth_for_credential(&cred);
        assert_eq!(header_name, "Authorization");
        assert_eq!(header_value, "Bearer rotated-key");
    }

    #[tokio::test]
    async fn test_set_credential_to_bearer_token() {
        let provider = OpenAiProvider::new("first-key").unwrap();
        provider
            .set_credential(crate::model_router::Credential::bearer_token("oauth-token"))
            .await
            .unwrap();

        let cred = provider.gateway_client.credential.read().await;
        let (header_name, header_value) = provider.gateway_client.auth_for_credential(&cred);
        assert_eq!(header_name, "Authorization");
        assert_eq!(header_value, "Bearer oauth-token");
    }
}
