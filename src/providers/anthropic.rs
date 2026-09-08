//! Anthropic provider implementation for Syscity
//!
//! Supports Claude 3/3.5 models with native Anthropic API format.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, instrument, warn};

use super::{
    stream_wrappers::ProviderStreamFamily, CompletionChunk, CompletionRequest, CompletionResponse,
    CompletionStream, FunctionCall, FunctionDefinition, Message, Provider, ProviderInstanceConfig,
    Role, ToolCall, Usage,
};
use crate::model_router::gateway_client::GatewayClient;
use crate::model_router::HttpGatewayClient;

/// Anthropic API client
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    /// Base URL
    base_url: String,
    /// Default model
    default_model: String,
    /// Unified HTTP client with retry/backoff, auth, and rate limiting
    gateway_client: std::sync::Arc<HttpGatewayClient>,
    /// Optional stream family override (e.g. for Kimi Anthropic endpoint)
    stream_family_override: Option<ProviderStreamFamily>,
    /// Maximum context length (0 = use model-based estimate).
    max_context: usize,
}

/// Anthropic API request body
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

/// Anthropic message format
#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<ContentBlock>,
}

/// Content block (text, image, or tool use)
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Anthropic image source format
#[derive(Debug, Serialize, Deserialize, Clone)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

/// Anthropic tool definition
#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

/// Anthropic API response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnthropicResponse {
    id: String,
    #[serde(rename = "type")]
    response_type: String,
    role: String,
    content: Vec<ContentBlock>,
    model: String,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

/// Anthropic usage statistics
#[derive(Debug, Deserialize, Default)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
}

impl AnthropicUsage {
    fn to_usage(&self) -> Usage {
        Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: self.input_tokens + self.output_tokens,
            cache_read_tokens: self.cache_read_input_tokens.unwrap_or(0),
            cache_creation_tokens: self.cache_creation_input_tokens.unwrap_or(0),
            x_credits_used: None,
            x_credit_balance: None,
        }
    }
}

/// Anthropic error response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnthropicError {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

/// Anthropic streaming event
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    message: Option<StreamMessage>,
    #[serde(default)]
    usage: Option<StreamUsage>,
    #[serde(default)]
    content_block: Option<StreamContentBlock>,
}

/// Content block in content_block_start streaming event
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

/// Delta in streaming response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Message start in streaming
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamMessage {
    usage: Option<StreamUsage>,
}

/// Usage in streaming
#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct StreamUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
}

/// Accumulates usage across a stream's SSE events. `message_start` carries
/// input (and cache) tokens; `message_delta` carries output tokens; the final
/// `message_stop` chunk reports the merged total.
#[derive(Debug, Default)]
struct UsageAccum {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
}

impl UsageAccum {
    fn merge(&mut self, u: &StreamUsage) {
        if let Some(v) = u.input_tokens {
            self.input_tokens = Some(v);
        }
        if let Some(v) = u.output_tokens {
            self.output_tokens = Some(v);
        }
        if let Some(v) = u.cache_creation_input_tokens {
            self.cache_creation_input_tokens = Some(v);
        }
        if let Some(v) = u.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(v);
        }
    }

    fn to_usage(&self) -> Option<Usage> {
        let input = self.input_tokens?;
        Some(Usage {
            prompt_tokens: input,
            completion_tokens: self.output_tokens.unwrap_or(0),
            total_tokens: input + self.output_tokens.unwrap_or(0),
            cache_read_tokens: self.cache_read_input_tokens.unwrap_or(0),
            cache_creation_tokens: self.cache_creation_input_tokens.unwrap_or(0),
            x_credits_used: None,
            x_credit_balance: None,
        })
    }
}

impl AnthropicProvider {
    /// Create a new Anthropic provider from an API key string
    /// (backward-compatible).
    pub fn new(api_key: impl Into<String>) -> crate::Result<Self> {
        Self::with_credential(crate::model_router::Credential::api_key(api_key))
    }

    /// Create with custom base URL from an API key string
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
        extra_headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        extra_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let gateway_client = std::sync::Arc::new(
            HttpGatewayClient::new(
                "https://api.anthropic.com",
                credential,
                Duration::from_secs(120),
            )?
            .with_api_key_header("x-api-key")
            .with_extra_headers(extra_headers),
        );

        Ok(Self {
            base_url: "https://api.anthropic.com".to_string(),
            default_model: "claude-3-5-sonnet-20241022".to_string(),
            gateway_client,
            stream_family_override: None,
            max_context: 0,
        })
    }

    /// Create from a fully-resolved `ProviderInstanceConfig`.
    ///
    /// This is the primary constructor used by the resolver; it sets all fields
    /// including protocol-variant-specific stream families (e.g., Kimi
    /// Anthropic).
    pub fn from_config(config: &ProviderInstanceConfig) -> crate::Result<Self> {
        let credential =
            crate::model_router::Credential::api_key(config.api_key.clone().unwrap_or_default());
        let mut this = Self::with_credential(credential)?;
        this.base_url = config.base_url.clone();
        this.default_model = config.model.clone();
        this.stream_family_override = Some(config.stream_family);
        this.max_context = config.max_context;
        Ok(this)
    }

    /// Set the default model
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Build the request URL
    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    /// Convert internal messages to Anthropic format
    fn to_anthropic_messages(messages: &[Message]) -> (Option<String>, Vec<AnthropicMessage>) {
        let mut system_prompt: Option<String> = None;
        let mut anthropic_messages = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    // System messages go in the system field, not messages array
                    system_prompt = Some(msg.content.clone());
                }
                Role::User => {
                    let content = if let Some(ref blocks) = msg.content_blocks {
                        blocks
                            .iter()
                            .map(|b| match b {
                                super::ContentBlock::Text { text } => {
                                    ContentBlock::Text { text: text.clone() }
                                }
                                super::ContentBlock::Image { base64, mime_type } => {
                                    ContentBlock::Image {
                                        source: ImageSource {
                                            source_type: "base64".to_string(),
                                            media_type: mime_type.clone(),
                                            data: base64.clone(),
                                        },
                                    }
                                }
                            })
                            .collect()
                    } else {
                        vec![ContentBlock::Text { text: msg.content.clone() }]
                    };
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content,
                    });
                }
                Role::Assistant => {
                    let mut content = if let Some(ref blocks) = msg.content_blocks {
                        blocks
                            .iter()
                            .map(|b| match b {
                                super::ContentBlock::Text { text } => {
                                    ContentBlock::Text { text: text.clone() }
                                }
                                super::ContentBlock::Image { base64, mime_type } => {
                                    ContentBlock::Image {
                                        source: ImageSource {
                                            source_type: "base64".to_string(),
                                            media_type: mime_type.clone(),
                                            data: base64.clone(),
                                        },
                                    }
                                }
                            })
                            .collect()
                    } else {
                        vec![ContentBlock::Text { text: msg.content.clone() }]
                    };

                    // Add tool calls if present
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tc in tool_calls {
                            content.push(ContentBlock::ToolUse {
                                id: tc.id.clone(),
                                name: tc.function.name.clone(),
                                input: serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or_default(),
                            });
                        }
                    }

                    anthropic_messages.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content,
                    });
                }
                Role::Tool => {
                    // Tool results are separate messages in Anthropic
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: msg.tool_call_id.clone().unwrap_or_default(),
                            content: msg.content.clone(),
                            is_error: None,
                        }],
                    });
                }
            }
        }

        (system_prompt, anthropic_messages)
    }

    /// Convert Anthropic response to internal format
    fn from_anthropic_response(response: AnthropicResponse) -> CompletionResponse {
        let mut text_content = String::new();
        let mut tool_calls = Vec::new();

        for block in &response.content {
            match block {
                ContentBlock::Text { text } => {
                    text_content.push_str(text);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id: id.clone(),
                        call_type: "tool_use".to_string(),
                        function: super::FunctionCall {
                            name: name.clone(),
                            arguments: input.to_string(),
                        },
                        index: None,
                        result: None,
                    });
                }
                _ => {}
            }
        }

        CompletionResponse {
            message: Message {
                role: Role::Assistant,
                content: text_content,
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
            usage: Some(response.usage.to_usage()),
            model: response.model,
            finish_reason: response.stop_reason,
        }
    }

    /// Convert FunctionDefinition to Anthropic tool
    fn to_anthropic_tool(func: &FunctionDefinition) -> AnthropicTool {
        AnthropicTool {
            name: func.name.clone(),
            description: func.description.clone(),
            input_schema: func.parameters.clone(),
        }
    }

    /// Parse Server-Sent Events (SSE) from a complete chunk of SSE data.
    ///
    /// Handles text deltas, tool_use start/delta/stop events, and message_stop.
    fn parse_sse_events(text: &str, usage_accum: &mut UsageAccum) -> Vec<CompletionChunk> {
        let mut chunks = Vec::new();
        // Tool call accumulation state across events within the same parse call
        struct ToolAccum {
            id: String,
            name: String,
            input: String,
        }
        let mut current_tool_call: Option<ToolAccum> = None;

        for line in text.lines() {
            let line = line.trim();

            // SSE events start with "data: "
            if let Some(data) = line.strip_prefix("data: ") {
                // Handle completion
                if data == "[DONE]" {
                    chunks.push(CompletionChunk {
                        content: None,
                        reasoning_content: None,
                        tool_calls: None,
                        is_done: true,
                        usage: None,
                    });
                    break;
                }

                // Parse the event JSON
                match serde_json::from_str::<StreamEvent>(data) {
                    Ok(event) => {
                        match event.event_type.as_str() {
                            "content_block_delta" => {
                                if let Some(delta) = event.delta {
                                    match delta.delta_type.as_deref() {
                                        Some("text_delta") => {
                                            if let Some(text) = delta.text {
                                                chunks.push(CompletionChunk {
                                                    content: Some(text),
                                                    reasoning_content: None,
                                                    tool_calls: None,
                                                    is_done: false,
                                                    usage: None,
                                                });
                                            }
                                        }
                                        Some("input_json_delta") => {
                                            if let Some(partial_json) = delta.partial_json {
                                                if let Some(ref mut tool) = current_tool_call {
                                                    tool.input.push_str(&partial_json);
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            "content_block_start" => {
                                if let Some(content_block) = &event.content_block {
                                    if content_block.block_type == "tool_use" {
                                        current_tool_call = Some(ToolAccum {
                                            id: content_block.id.clone().unwrap_or_default(),
                                            name: content_block.name.clone().unwrap_or_default(),
                                            input: String::new(),
                                        });
                                    }
                                }
                                // Text content blocks don't have a start delta;
                                // text comes through content_block_delta
                            }
                            "content_block_stop" => {
                                if let Some(tool) = current_tool_call.take() {
                                    let args =
                                        serde_json::from_str(&tool.input).unwrap_or_else(|e| {
                                            warn!("Failed to parse tool input JSON: {}", e);
                                            serde_json::Value::Object(serde_json::Map::new())
                                        });
                                    chunks.push(CompletionChunk {
                                        content: None,
                                        reasoning_content: None,
                                        tool_calls: Some(vec![ToolCall {
                                            id: tool.id,
                                            call_type: "function".to_string(),
                                            function: FunctionCall {
                                                name: tool.name,
                                                arguments: serde_json::to_string(&args)
                                                    .unwrap_or_default(),
                                            },
                                            index: None,
                                            result: None,
                                        }]),
                                        is_done: false,
                                        usage: None,
                                    });
                                }
                            }
                            "message_stop" => {
                                chunks.push(CompletionChunk {
                                    content: None,
                                    reasoning_content: None,
                                    tool_calls: None,
                                    is_done: true,
                                    usage: usage_accum.to_usage(),
                                });
                            }
                            "message_start" => {
                                if let Some(message) = &event.message {
                                    if let Some(u) = &message.usage {
                                        usage_accum.merge(u);
                                    }
                                }
                            }
                            "message_delta" => {
                                // Carries output_tokens (and stop_reason)
                                if let Some(u) = &event.usage {
                                    usage_accum.merge(u);
                                }
                            }
                            _ => {
                                // Ignore other event types (message_start,
                                // ping, etc.)
                            }
                        }
                    }
                    Err(e) => {
                        debug!("Failed to parse stream event: {} - {}", e, data);
                    }
                }
            }
        }

        chunks
    }

    /// Parse a raw byte chunk from the HTTP response body, buffering partial
    /// SSE lines across calls. Returns completed SSE lines.
    fn parse_sse_chunk(
        incoming: &str,
        buffer: &mut String,
        usage_accum: &mut UsageAccum,
    ) -> Vec<CompletionChunk> {
        buffer.push_str(incoming);

        // Find the last complete SSE event boundary (ends with "\n\n" or "\n")
        // SSE events end with a blank line, but each data line is "\ndata: ..."
        let last_boundary = buffer.rfind("\n\n").map(|i| i + 2).unwrap_or(0);

        // Also handle the case where we have a complete line ending with \n
        // but no blank line yet — process any complete data lines
        let complete = if last_boundary > 0 {
            let ready = buffer[..last_boundary].to_string();
            buffer.drain(..last_boundary);
            ready
        } else {
            // No complete SSE event yet; check for a complete data line
            let mut ready = String::new();
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].to_string();
                buffer.drain(..=newline_pos);
                if line.starts_with("data: ") {
                    ready.push_str(&line);
                    ready.push('\n');
                }
            }
            ready
        };

        if complete.is_empty() {
            Vec::new()
        } else {
            Self::parse_sse_events(&complete, usage_accum)
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn stream_family(&self) -> ProviderStreamFamily {
        if let Some(family) = self.stream_family_override {
            return family;
        }
        if self.default_model.contains("thinking") {
            ProviderStreamFamily::AnthropicThinking
        } else {
            ProviderStreamFamily::Anthropic
        }
    }

    #[instrument(skip(self, request))]
    async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse> {
        self.gateway_client.refresh_credential_if_needed().await?;

        let (system, messages) = Self::to_anthropic_messages(&request.messages);

        let tools = request.tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|t| Self::to_anthropic_tool(&t.function))
                .collect::<Vec<_>>()
        });

        let anthropic_request = AnthropicRequest {
            model: request.model.unwrap_or_else(|| self.default_model.clone()),
            max_tokens: request.max_tokens.unwrap_or(4096),
            system,
            messages,
            tools,
            temperature: request.temperature,
            stream: Some(request.stream),
        };

        // Merge provider-specific extra parameters
        let mut body_value = serde_json::to_value(&anthropic_request)?;
        crate::providers::merge_extra(&mut body_value, request.extra);

        debug!("Sending request to Anthropic API");

        let request_url = self.url("/v1/messages");

        let body = self
            .gateway_client
            .post_json_text(&request_url, &body_value)
            .await?;

        debug!("Received response from Anthropic API");

        let anthropic_response: AnthropicResponse = serde_json::from_str(&body).map_err(|e| {
            crate::error::SyscityError::ExternalService {
                source: format!("Failed to parse Anthropic response: {}", e),
                cause: Some(Box::new(e)),
            }
        })?;

        Ok(Self::from_anthropic_response(anthropic_response))
    }

    async fn stream(&self, request: CompletionRequest) -> crate::Result<CompletionStream> {
        self.gateway_client.refresh_credential_if_needed().await?;

        let (system, messages) = Self::to_anthropic_messages(&request.messages);

        let anthropic_request = AnthropicRequest {
            model: request.model.unwrap_or_else(|| self.default_model.clone()),
            max_tokens: request.max_tokens.unwrap_or(4096),
            system,
            messages,
            tools: request.tools.as_ref().map(|tools| {
                tools
                    .iter()
                    .map(|t| Self::to_anthropic_tool(&t.function))
                    .collect::<Vec<_>>()
            }),
            temperature: request.temperature,
            stream: Some(true),
        };

        // Merge provider-specific extra parameters
        let mut body_value = serde_json::to_value(&anthropic_request)?;
        crate::providers::merge_extra(&mut body_value, request.extra);

        let request_url = format!("{}/v1/messages", self.base_url);

        let response = self
            .gateway_client
            .post_json_streaming(&request_url, &body_value)
            .await?;

        // Process the stream as SSE events with line buffering
        // to handle TCP fragmentation.
        let body_stream = response.bytes_stream();
        let stream = async_stream::stream! {
            let mut line_buffer = String::new();
            let mut usage_accum = UsageAccum::default();
            for await chunk in body_stream {
                match chunk {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        for event in Self::parse_sse_chunk(&text, &mut line_buffer, &mut usage_accum) {
                            yield event;
                        }
                    }
                    Err(e) => {
                        error!("Stream error: {}", e);
                        yield CompletionChunk {
                            content: Some(format!("[Stream error: {}]", e)),
                            reasoning_content: None,
                            tool_calls: None,
                            is_done: true,
                            usage: None,
                        };
                    }
                }
            }
            // Process any remaining data in the buffer
            if !line_buffer.is_empty() {
                for event in Self::parse_sse_events(&line_buffer, &mut usage_accum) {
                    yield event;
                }
            }
        };

        return Ok(Box::pin(stream));
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn max_context(&self) -> usize {
        if self.max_context > 0 {
            self.max_context
        } else {
            200000 // Claude 3.5 Sonnet context window
        }
    }

    async fn health_check(&self) -> crate::Result<bool> {
        // Simple health check by making a minimal request
        let request = CompletionRequest {
            messages: vec![Message::user("Hi")],
            model: Some(self.default_model.clone()),
            max_tokens: Some(1),
            temperature: None,
            stream: false,
            tools: None,
            stop: None,
            extra: None,
            ..Default::default()
        };

        match self.complete(request).await {
            Ok(_) => Ok(true),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_provider_creation() {
        let provider = AnthropicProvider::new("test-key").unwrap();
        assert_eq!(provider.name(), "anthropic");
        assert!(provider.supports_tools());
    }

    #[test]
    fn test_to_anthropic_messages() {
        let messages = vec![
            Message::system("You are helpful"),
            Message::user("Hello"),
            Message::assistant("Hi there!"),
        ];

        let (system, anthropic_msgs) = AnthropicProvider::to_anthropic_messages(&messages);

        assert_eq!(system, Some("You are helpful".to_string()));
        assert_eq!(anthropic_msgs.len(), 2);
        assert_eq!(anthropic_msgs[0].role, "user");
        assert_eq!(anthropic_msgs[1].role, "assistant");
    }

    #[test]
    fn test_to_anthropic_messages_with_image() {
        let messages = vec![Message::user("").with_content_blocks(vec![
            crate::providers::ContentBlock::text("What is this?"),
            crate::providers::ContentBlock::image_base64("abc123", "image/png"),
        ])];

        let (system, anthropic_msgs) = AnthropicProvider::to_anthropic_messages(&messages);

        assert_eq!(system, None);
        assert_eq!(anthropic_msgs.len(), 1);
        assert_eq!(anthropic_msgs[0].role, "user");
        assert_eq!(anthropic_msgs[0].content.len(), 2);
        assert!(matches!(
            &anthropic_msgs[0].content[0],
            ContentBlock::Text { text } if text == "What is this?"
        ));
        assert!(matches!(
            &anthropic_msgs[0].content[1],
            ContentBlock::Image { source } if source.source_type == "base64" && source.media_type == "image/png" && source.data == "abc123"
        ));
    }

    #[tokio::test]
    async fn test_set_credential_updates_api_key() {
        let provider = AnthropicProvider::new("first-key").unwrap();
        provider
            .set_credential(crate::model_router::Credential::api_key("rotated-key"))
            .await
            .unwrap();

        let cred = provider.gateway_client.credential.read().await;
        let (header_name, header_value) = provider.gateway_client.auth_for_credential(&cred);
        assert_eq!(header_name, "x-api-key");
        assert_eq!(header_value, "rotated-key");
    }

    #[tokio::test]
    async fn test_set_credential_to_bearer_token() {
        let provider = AnthropicProvider::new("first-key").unwrap();
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
    // parse_sse_events tests
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_sse_text_delta() {
        let sse = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, Some("Hello".to_string()));
        assert!(!chunks[0].is_done);
    }

    #[test]
    fn test_parse_sse_message_stop() {
        let sse = "data: {\"type\":\"message_stop\"}\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_done);
        assert!(chunks[0].content.is_none());
    }

    #[test]
    fn test_parse_sse_done_signal() {
        let sse = "data: [DONE]\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_done);
    }

    #[test]
    fn test_parse_sse_unknown_event_ignored() {
        let sse = "data: {\"type\":\"ping\"}\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_parse_sse_malformed_json_ignored() {
        let sse = "data: not-json\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_parse_sse_tool_use_full_flow() {
        let sse = "\
data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"read_file\"}}\n\n
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/tmp\\\"\"}}\n\n
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"}\"}}\n\n
data: {\"type\":\"content_block_stop\"}\n\n
data: {\"type\":\"message_stop\"}\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        // Content_block_stop yields a tool call chunk, message_stop yields a done chunk
        let tool_chunks: Vec<_> = chunks.iter().filter(|c| c.tool_calls.is_some()).collect();
        assert_eq!(tool_chunks.len(), 1);
        let calls = tool_chunks[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "tu_1");
        assert_eq!(calls[0].function.name, "read_file");
        // Arguments should be valid JSON after accumulation
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["path"], "/tmp");

        let done_chunks: Vec<_> = chunks.iter().filter(|c| c.is_done).collect();
        assert_eq!(done_chunks.len(), 1);
    }

    #[test]
    fn test_parse_sse_multiple_text_deltas() {
        let sse = "\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \"}}\n\n
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n
data: {\"type\":\"message_stop\"}\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        let text_chunks: Vec<_> = chunks.iter().filter_map(|c| c.content.clone()).collect();
        assert_eq!(text_chunks, vec!["Hello ".to_string(), "world".to_string()]);
    }

    #[test]
    fn test_parse_sse_message_delta_ignored() {
        let sse = "data: {\"type\":\"message_delta\"}\n\n";
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut UsageAccum::default());
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_parse_sse_streaming_usage_with_cache() {
        let sse = "\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":60,\"cache_creation_input_tokens\":10}}}\n\n
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":20}}\n\n
data: {\"type\":\"message_stop\"}\n\n";
        let mut accum = UsageAccum::default();
        let chunks = AnthropicProvider::parse_sse_events(sse, &mut accum);
        let done = chunks.iter().find(|c| c.is_done).unwrap();
        let usage = done.usage.expect("message_stop should carry usage");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 120);
        assert_eq!(usage.cache_read_tokens, 60);
        assert_eq!(usage.cache_creation_tokens, 10);
    }

    #[test]
    fn test_parse_sse_usage_split_across_calls() {
        // Usage accumulates across separate parse calls (TCP fragmentation).
        let mut accum = UsageAccum::default();
        AnthropicProvider::parse_sse_events(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_read_input_tokens\":42}}}\n\n",
            &mut accum,
        );
        let chunks = AnthropicProvider::parse_sse_events(
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            &mut accum,
        );
        let done = chunks.iter().find(|c| c.is_done).unwrap();
        let usage = done.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.cache_read_tokens, 42);
    }
}
