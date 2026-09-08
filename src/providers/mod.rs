//! LLM Provider abstractions for Syscity
//!
//! This module defines the `Provider` trait for interacting with various LLM
//! services (OpenAI, Anthropic, Local models, etc.).
// INVARIANTS-NONE: API clients are stateless per request.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A message role in a conversation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System instructions to the model
    System,
    /// User input
    User,
    /// Assistant response
    Assistant,
    /// Tool output
    Tool,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::System => write!(f, "system"),
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
            Role::Tool => write!(f, "tool"),
        }
    }
}

/// A content block within a message (text or image).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ContentBlock {
    /// Plain text content
    Text { text: String },
    /// Base64-encoded image
    Image {
        /// Base64-encoded image data (without the data URI prefix).
        base64: String,
        /// MIME type, e.g. `image/png`.
        mime_type: String,
    },
}

impl ContentBlock {
    /// Create a text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Create an image block from base64 data.
    pub fn image_base64(base64: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            base64: base64.into(),
            mime_type: mime_type.into(),
        }
    }
}

/// A single message in a conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// The role of the message sender
    pub role: Role,
    /// The content of the message
    pub content: String,
    /// Optional multimodal content blocks (override `content` when present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_blocks: Option<Vec<ContentBlock>>,
    /// Optional reasoning / thinking content (e.g. from reasoning models)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Optional name (for tool calls or multi-user scenarios)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional tool calls (for assistant messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Optional tool call ID (for tool messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

impl Message {
    /// Create a new system message
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            metadata: None,
        }
    }

    /// Create a new user message
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            metadata: None,
        }
    }

    /// Create a new user message with an identifiable name (for multi-user /
    /// group chats).
    pub fn user_named(name: impl Into<String>, content: impl Into<String>) -> Self {
        let mut msg = Self::user(content);
        msg.name = Some(name.into());
        msg
    }

    /// Create a new assistant message
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: None,
            metadata: None,
        }
    }

    /// Create a new tool message
    pub fn tool(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            content_blocks: None,
            reasoning_content: None,
            name: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            metadata: None,
        }
    }

    /// Add a name to the message
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Add tool calls to the message
    pub fn with_tool_calls(mut self, calls: Vec<ToolCall>) -> Self {
        self.tool_calls = Some(calls);
        self
    }

    /// Add metadata to the message
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value.into());
        self
    }

    /// Replace content with multimodal content blocks.
    pub fn with_content_blocks(mut self, blocks: Vec<ContentBlock>) -> Self {
        self.content_blocks = Some(blocks);
        self
    }

    /// Add an image block to this message.
    ///
    /// If the message currently has no content blocks, the existing `content`
    /// text is automatically converted into a text block so that both text
    /// and image are sent together.
    pub fn with_image(mut self, base64: impl Into<String>, mime_type: impl Into<String>) -> Self {
        let block = ContentBlock::image_base64(base64, mime_type);
        match self.content_blocks {
            Some(ref mut blocks) => blocks.push(block),
            None => {
                let mut blocks = vec![ContentBlock::text(self.content.clone())];
                blocks.push(block);
                self.content_blocks = Some(blocks);
            }
        }
        self
    }

    /// Whether this message contains any image blocks.
    pub fn has_images(&self) -> bool {
        self.content_blocks
            .as_ref()
            .map(|blocks| {
                blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Image { .. }))
            })
            .unwrap_or(false)
    }

    /// Collect all text from content blocks, falling back to `content`.
    pub fn all_text(&self) -> String {
        match &self.content_blocks {
            Some(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            None => self.content.clone(),
        }
    }
}

/// A tool call from the assistant
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique ID for this tool call
    pub id: String,
    /// The type of tool call (typically "function")
    pub call_type: String,
    /// The function to call
    pub function: FunctionCall,
    /// Streaming index (position in the tool_calls array); set only during
    /// streaming deltas
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    /// Tool execution result (populated after execution, persisted for history
    /// replay)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// A function call within a tool call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The name of the function
    pub name: String,
    /// The arguments as a JSON string
    pub arguments: String,
}

/// The result of a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// The ID of the tool call this is a result for
    pub tool_call_id: String,
    /// The role of the result (typically "tool")
    pub role: Role,
    /// The content (result) of the tool execution
    pub content: String,
    /// Whether the tool execution was successful
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl ToolResult {
    /// Create a successful tool result
    pub fn success(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            role: Role::Tool,
            content: content.into(),
            is_error: Some(false),
        }
    }

    /// Create an error tool result
    pub fn error(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            role: Role::Tool,
            content: content.into(),
            is_error: Some(true),
        }
    }
}

/// A chunk of a streaming response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunk {
    /// The content delta for this chunk
    pub content: Option<String>,
    /// Reasoning / thinking content delta for this chunk
    pub reasoning_content: Option<String>,
    /// Tool calls being streamed
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Whether this is the final chunk
    pub is_done: bool,
    /// Usage statistics (only in final chunk)
    pub usage: Option<Usage>,
}

/// Usage statistics for a completion
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Number of tokens in the prompt
    pub prompt_tokens: u32,
    /// Number of tokens in the completion
    pub completion_tokens: u32,
    /// Total number of tokens
    pub total_tokens: u32,
    /// Tokens read from the provider-side prompt cache (0 if unsupported)
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// Tokens written to the provider-side prompt cache
    #[serde(default)]
    pub cache_creation_tokens: u32,
    /// Credits billed by the cloud relay for this call (top-level
    /// `x_credits_used` extension on OpenAI-compatible bodies; `None` when
    /// the provider does not report it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_credits_used: Option<i64>,
    /// Credit balance reported by the cloud relay after this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_credit_balance: Option<i64>,
}

/// A request for text completion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// The conversation history
    pub messages: Vec<Message>,
    /// Available tools
    pub tools: Option<Vec<ToolDefinition>>,
    /// Model parameters
    pub temperature: Option<f32>,
    /// Maximum tokens to generate
    pub max_tokens: Option<u32>,
    /// Whether to stream the response
    pub stream: bool,
    /// The specific model to use
    pub model: Option<String>,
    /// Stop sequences
    pub stop: Option<Vec<String>>,
    /// Provider-specific extra parameters (e.g. thinking, top_p, etc.)
    pub extra: Option<serde_json::Value>,
    /// Whether the request requires vision capability (image input).
    pub requires_vision: bool,
    /// Whether the request requires tool calling capability.
    pub requires_tools: bool,
    /// Whether the request requires reasoning / thinking capability.
    pub requires_reasoning: bool,
    /// Fallback models to try if the primary model fails.
    pub fallback_models: Vec<String>,
}

impl Default for CompletionRequest {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            tools: None,
            temperature: Some(0.7),
            max_tokens: Some(2048),
            stream: false,
            model: None,
            stop: None,
            extra: None,
            requires_vision: false,
            requires_tools: false,
            requires_reasoning: false,
            fallback_models: Vec::new(),
        }
    }
}

/// A response from a completion request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// The message generated by the model
    pub message: Message,
    /// Usage statistics
    pub usage: Option<Usage>,
    /// The model used
    pub model: String,
    /// Finish reason
    pub finish_reason: Option<String>,
}

/// Definition of a tool for the model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// The type of tool (typically "function")
    #[serde(rename = "type")]
    pub tool_type: String,
    /// The function definition
    pub function: FunctionDefinition,
}

/// Definition of a function tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    /// The name of the function
    pub name: String,
    /// A description of what the function does
    pub description: String,
    /// The parameters schema (JSON Schema)
    pub parameters: serde_json::Value,
}

/// A stream of completion chunks
pub type CompletionStream =
    std::pin::Pin<Box<dyn tokio_stream::Stream<Item = CompletionChunk> + Send>>;

/// Trait for LLM providers
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get the name of this provider
    fn name(&self) -> &str;

    /// Get the default model for this provider
    fn default_model(&self) -> &str;

    /// Check if this provider supports tool calling
    fn supports_tools(&self) -> bool;

    /// Get the maximum context size for this provider
    fn max_context(&self) -> usize;

    /// Complete a request (non-streaming)
    async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse>;

    /// Stream a completion
    async fn stream(&self, request: CompletionRequest) -> crate::Result<CompletionStream>;

    /// Return the stream family for this provider.
    ///
    /// Used by the `StreamFamilyRegistry` to apply provider-specific wrappers.
    fn stream_family(&self) -> stream_wrappers::ProviderStreamFamily {
        stream_wrappers::ProviderStreamFamily::Generic
    }

    /// Count tokens in messages (approximate if not provided by API)
    fn count_tokens(&self, messages: &[Message]) -> usize {
        // Simple approximation: 4 chars per token on average
        messages.iter().map(|m| m.content.len() / 4).sum()
    }

    /// Check if the provider is healthy
    async fn health_check(&self) -> crate::Result<bool>;

    /// Update the credential used by this provider at runtime.
    ///
    /// Implementations should replace the current credential with the supplied
    /// one so that subsequent requests use the new credential without needing
    /// to rebuild the provider or mutate the original `ProviderConfig`.
    async fn set_credential(
        &self,
        credential: crate::model_router::Credential,
    ) -> crate::Result<()>;

    /// Optional per-request timeout in seconds.
    ///
    /// Return `None` (the default) to use the system-wide timeout.
    fn timeout(&self) -> Option<Duration> {
        None
    }
}

/// Registry of providers
#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn Provider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self { providers: HashMap::new() }
    }

    /// Register a provider
    pub fn register(&mut self, provider: Box<dyn Provider>) {
        let name = provider.name().to_string();
        self.providers.insert(name, provider);
    }

    /// Get a provider by name
    pub fn get(&self, name: &str) -> Option<&dyn Provider> {
        self.providers.get(name).map(|p| p.as_ref())
    }

    /// List available provider names
    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// Check if a provider exists
    pub fn has(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    /// Remove a provider by name, returning the removed provider if it existed.
    pub fn remove(&mut self, name: &str) -> Option<Box<dyn Provider>> {
        self.providers.remove(name)
    }
}

pub mod anthropic;
pub mod fallback;
pub mod gemini;
pub mod mock;
pub mod openai;
pub mod preset;
pub mod resolver;
pub mod sdk;
pub mod stream_wrappers;

pub use anthropic::AnthropicProvider;
pub use fallback::{FallbackChainBuilder, FallbackProvider};
pub use gemini::GeminiProvider;
/// Re-export the programmable mock provider for tests (unit + integration).
pub use mock::MockProvider;
pub use openai::OpenAiProvider;
pub use sdk::{ProviderCapabilities, ProviderHealth, ProviderMetadata, ProviderPack, ProviderSdk};

// ──── Protocol & Provider Architecture Types ────

/// Supported API protocols.
///
/// Each protocol has its own request/response format and SSE event structure.
/// Only 3 protocols exist; vendors map to one of these via presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OpenAI Chat Completions API (SSE delta format, /chat/completions)
    OpenAi,
    /// Anthropic Messages API (content_block_delta events, /v1/messages)
    Anthropic,
    /// Google Gemini API (parts[], generateContent)
    Gemini,
}

/// Authentication method for a provider endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Authorization: Bearer {key}
    Bearer,
    /// x-api-key: {key}
    ApiKeyHeader,
    /// x-goog-api-key: {key}
    GoogleApiKey,
    /// No authentication (local services like Ollama)
    None,
    /// Custom header name
    CustomHeader { name: String },
}

/// A protocol variant within a provider definition.
///
/// A single vendor (e.g. Kimi) may expose multiple protocol endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolVariant {
    /// The API protocol
    pub protocol: Protocol,
    /// Default base URL for this variant
    pub default_base_url: String,
    /// Default model name
    pub default_model: String,
    /// Authentication method
    pub auth_method: AuthMethod,
    /// List-models endpoint path relative to base_url (e.g. "/models").
    /// When absent, the path is derived from the protocol.
    #[serde(default)]
    pub models_endpoint: Option<String>,
    /// Default max context length
    pub default_max_context: usize,
    /// Whether vision is supported by default
    pub default_supports_vision: bool,
    /// Whether tool calling is supported by default
    pub default_supports_tools: bool,
    /// Default stream family for this variant
    pub default_stream_family: stream_wrappers::ProviderStreamFamily,
}

/// Definition of a provider vendor (preset).
///
/// Each vendor has one or more protocol variants. For example, Kimi supports
/// both OpenAI-compatible and Anthropic-compatible endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDefinition {
    /// Configuration key name (e.g. "kimi", "openai")
    pub name: String,
    /// Human-readable display name
    pub display_name: String,
    /// Available protocol variants
    pub variants: Vec<ProtocolVariant>,
}

/// Runtime configuration that drives a protocol-level provider.
///
/// This is the fully-resolved configuration after merging preset defaults
/// with user overrides. It is passed directly to protocol providers.
#[derive(Debug, Clone)]
pub struct ProviderInstanceConfig {
    /// The protocol to use
    pub protocol: Protocol,
    /// Authentication method
    pub auth_method: AuthMethod,
    /// API key (if applicable)
    pub api_key: Option<String>,
    /// Base URL for the API endpoint
    pub base_url: String,
    /// Model name
    pub model: String,
    /// Max context length
    pub max_context: usize,
    /// Whether vision is supported
    pub supports_vision: bool,
    /// Whether tool calling is supported
    pub supports_tools: bool,
    /// Stream family for wrapper selection
    pub stream_family: stream_wrappers::ProviderStreamFamily,
}

/// Merge provider-specific `extra` parameters into a serialized request body.
///
/// Both `body` and `extra` are expected to be JSON objects. Non-object values
/// (including `None`) are silently ignored so this is safe to call
/// unconditionally.
pub fn merge_extra(body: &mut serde_json::Value, extra: Option<serde_json::Value>) {
    let Some(serde_json::Value::Object(extra_map)) = extra else {
        return;
    };
    if let serde_json::Value::Object(ref mut map) = body {
        map.extend(extra_map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_creation() {
        let system = Message::system("You are a helpful assistant");
        assert_eq!(system.role, Role::System);
        assert_eq!(system.content, "You are a helpful assistant");

        let user = Message::user("Hello!");
        assert_eq!(user.role, Role::User);
        assert_eq!(user.content, "Hello!");

        let assistant = Message::assistant("Hi there!");
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.content, "Hi there!");
    }

    #[test]
    fn test_tool_result() {
        let success = ToolResult::success("call_123", "Result data");
        assert_eq!(success.tool_call_id, "call_123");
        assert_eq!(success.is_error, Some(false));

        let error = ToolResult::error("call_456", "Something went wrong");
        assert_eq!(error.is_error, Some(true));
    }

    #[test]
    fn test_provider_registry() {
        let registry = ProviderRegistry::new();
        assert!(registry.list().is_empty());

        // We can't easily test with real providers, but we can test the interface
        assert!(!registry.has("test"));
        assert!(registry.get("test").is_none());
    }

    #[test]
    fn test_role_display() {
        assert_eq!(format!("{}", Role::System), "system");
        assert_eq!(format!("{}", Role::User), "user");
        assert_eq!(format!("{}", Role::Assistant), "assistant");
        assert_eq!(format!("{}", Role::Tool), "tool");
    }

    #[test]
    fn test_role_serialization() {
        let json = serde_json::to_string(&Role::User).unwrap();
        assert_eq!(json, "\"user\"");
        let de: Role = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(de, Role::Assistant);
    }

    #[test]
    fn test_message_user_named() {
        let msg = Message::user_named("Alice", "Hello!");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "Hello!");
        assert_eq!(msg.name, Some("Alice".to_string()));
    }

    #[test]
    fn test_message_tool() {
        let msg = Message::tool("result data", "call_123");
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.content, "result data");
        assert_eq!(msg.tool_call_id, Some("call_123".to_string()));
    }

    #[test]
    fn test_message_with_name() {
        let msg = Message::system("Instructions").with_name("config");
        assert_eq!(msg.name, Some("config".to_string()));
    }

    #[test]
    fn test_message_with_tool_calls() {
        let calls = vec![ToolCall {
            id: "c1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            },
            index: None,
            result: None,
        }];
        let msg = Message::assistant("Using tool").with_tool_calls(calls.clone());
        assert_eq!(msg.tool_calls.as_ref().unwrap()[0].id, "c1");
    }

    #[test]
    fn test_message_with_metadata() {
        let msg = Message::user("Hi")
            .with_metadata("source", "telegram")
            .with_metadata("chat_id", "12345");
        let meta = msg.metadata.unwrap();
        assert_eq!(meta.get("source"), Some(&"telegram".to_string()));
        assert_eq!(meta.get("chat_id"), Some(&"12345".to_string()));
    }

    #[test]
    fn test_message_serialization() {
        let msg = Message::user("Hello");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("Hello"));
        assert!(json.contains("user"));
    }

    #[test]
    fn test_message_with_image() {
        let msg = Message::user("describe this").with_image("iVBORw0KGgo=", "image/png");
        assert!(msg.has_images());
        assert_eq!(msg.content_blocks.as_ref().unwrap().len(), 2);
        assert_eq!(msg.all_text(), "describe this");
    }

    #[test]
    fn test_message_with_content_blocks() {
        let msg = Message::user("").with_content_blocks(vec![
            ContentBlock::text("What do you see?"),
            ContentBlock::image_base64("abc123", "image/jpeg"),
        ]);
        assert!(msg.has_images());
        assert_eq!(msg.content_blocks.as_ref().unwrap().len(), 2);
        assert_eq!(msg.all_text(), "What do you see?");
    }

    #[test]
    fn test_message_no_images() {
        let msg = Message::user("Just text");
        assert!(!msg.has_images());
        assert_eq!(msg.all_text(), "Just text");
    }

    #[test]
    fn test_content_block_creation() {
        let text = ContentBlock::text("hello");
        assert!(matches!(text, ContentBlock::Text { text } if text == "hello"));

        let img = ContentBlock::image_base64("data", "image/png");
        assert!(
            matches!(img, ContentBlock::Image { base64, mime_type } if base64 == "data" && mime_type == "image/png")
        );
    }

    #[test]
    fn test_tool_call_creation() {
        let tc = ToolCall {
            id: "tc1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "grep".to_string(),
                arguments: "{\"pattern\": \"foo\"}".to_string(),
            },
            index: None,
            result: None,
        };
        assert_eq!(tc.id, "tc1");
        assert_eq!(tc.function.name, "grep");
    }

    #[test]
    fn test_tool_result_success() {
        let tr = ToolResult::success("id1", "all good");
        assert_eq!(tr.tool_call_id, "id1");
        assert_eq!(tr.content, "all good");
        assert_eq!(tr.is_error, Some(false));
        assert_eq!(tr.role, Role::Tool);
    }

    #[test]
    fn test_tool_result_error() {
        let tr = ToolResult::error("id2", "failed");
        assert_eq!(tr.is_error, Some(true));
    }

    #[test]
    fn test_usage_default() {
        let u = Usage::default();
        assert_eq!(u.prompt_tokens, 0);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.total_tokens, 0);
    }

    #[test]
    fn test_completion_request_default() {
        let req = CompletionRequest::default();
        assert!(req.messages.is_empty());
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(2048));
        assert!(!req.stream);
    }

    #[test]
    fn test_completion_chunk() {
        let chunk = CompletionChunk {
            content: Some("hi".to_string()),
            reasoning_content: None,
            tool_calls: None,
            is_done: false,
            usage: None,
        };
        assert_eq!(chunk.content, Some("hi".to_string()));
        assert!(!chunk.is_done);
    }

    #[test]
    fn test_tool_definition() {
        let td = ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "test".to_string(),
                description: "A test".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        assert_eq!(td.tool_type, "function");
        let json = serde_json::to_string(&td).unwrap();
        assert!(json.contains("test"));
    }

    #[test]
    fn test_provider_registry_default() {
        let registry: ProviderRegistry = Default::default();
        assert!(registry.list().is_empty());
    }

    // Mock provider for registry tests
    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        fn supports_tools(&self) -> bool {
            true
        }
        fn max_context(&self) -> usize {
            4096
        }
        async fn complete(&self, _request: CompletionRequest) -> crate::Result<CompletionResponse> {
            Ok(CompletionResponse {
                message: Message::assistant("mock completion response".to_string()),
                model: "mock-model".to_string(),
                usage: None,
                finish_reason: Some("stop".to_string()),
            })
        }
        async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(CompletionChunk {
                content: Some("mock streaming response".to_string()),
                reasoning_content: None,
                tool_calls: None,
                is_done: true,
                usage: None,
            });
            Ok(Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)))
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
    }

    #[test]
    fn test_provider_registry_register_and_get() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider));
        assert!(registry.has("mock"));
        assert_eq!(registry.list(), vec!["mock"]);
        let p = registry.get("mock").unwrap();
        assert_eq!(p.name(), "mock");
        assert_eq!(p.default_model(), "mock-model");
        assert!(p.supports_tools());
        assert_eq!(p.max_context(), 4096);
    }

    #[test]
    fn test_provider_registry_remove() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider));
        assert!(registry.has("mock"));
        let removed = registry.remove("mock");
        assert!(removed.is_some());
        assert!(!registry.has("mock"));
        assert!(registry.remove("nonexistent").is_none());
    }

    #[test]
    fn test_provider_registry_debug() {
        let mut registry = ProviderRegistry::new();
        registry.register(Box::new(MockProvider));
        let dbg = format!("{:?}", registry);
        assert!(dbg.contains("ProviderRegistry"));
        assert!(dbg.contains("mock"));
    }

    #[test]
    fn test_provider_count_tokens() {
        let provider = MockProvider;
        let msgs = vec![
            Message::system("You are helpful"),
            Message::user("Hello there"),
        ];
        let tokens = provider.count_tokens(&msgs);
        // count_tokens sums per-message: 15/4 + 11/4 = 3 + 2 = 5
        assert_eq!(tokens, 5);
    }
}
