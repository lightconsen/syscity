//! Programmable mock LLM provider for testing.
//!
//! Use `MockProvider` to simulate LLM responses without making real API calls.
//! It supports both **predefined response sequences** and **dynamic callbacks**
//! driven by the conversation history, plus request history inspection for
//! post-test assertions.
//!
//! # Example: fixed response sequence
//!
//! ```
//! use syscity::providers::mock::MockProvider;
//! use syscity::providers::Message;
//!
//! let mock = MockProvider::new().with_responses(vec![
//!     Message::assistant("I'll help you with that."),
//!     Message::assistant("Done!"),
//! ]);
//! ```
//!
//! # Example: callback-driven (multi-turn tool chain)
//!
//! ```
//! use syscity::providers::mock::MockProvider;
//! use syscity::providers::{FunctionCall, Message, Role, ToolCall};
//!
//! let mock = MockProvider::new().with_callback(|messages| {
//!     // Look at the conversation so far and decide what to return
//!     let has_tool_result = messages.iter().any(|m| m.role == Role::Tool);
//!     if has_tool_result {
//!         Message::assistant("Based on the file content, here's my answer.")
//!     } else {
//!         Message::assistant("Let me read the file.").with_tool_calls(vec![ToolCall {
//!             id: "call_1".to_string(),
//!             call_type: "function".to_string(),
//!             function: FunctionCall {
//!                 name: "file_read".to_string(),
//!                 arguments: r#"{"path": "/tmp/test.txt"}"#.to_string(),
//!             },
//!             index: None,
//!             result: None,
//!         }])
//!     }
//! });
//! ```

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracing::warn;

use crate::error::SyscityError;

use super::{
    CompletionChunk, CompletionRequest, CompletionResponse, CompletionStream, Message, Provider,
    Usage,
};

/// Callback signature for dynamic mock responses.
type MockCallback = Box<dyn Fn(&[Message]) -> Message + Send + Sync>;

/// Callback signature for injecting a provider error before a response is
/// resolved. Returning `Some` fails the call with that error; `None` proceeds
/// normally.
type MockErrorCallback = Box<dyn Fn(&[Message]) -> Option<SyscityError> + Send + Sync>;

/// Internal mutable state for the mock provider.
struct MockState {
    /// Predefined responses returned in order.
    responses: Vec<Message>,
    /// Current position in `responses`.
    index: usize,
    /// Optional dynamic callback that inspects the conversation history.
    callback: Option<MockCallback>,
    /// Optional callback that can fail the call (e.g. simulate a
    /// context-length rejection) before a response is resolved.
    error_callback: Option<MockErrorCallback>,
    /// Record of every `CompletionRequest` received.
    history: Vec<CompletionRequest>,
}

/// A programmable mock LLM provider for testing agent behaviour without
/// real API calls.
///
/// Clone is cheap (shares the same `Arc<Mutex<MockState>>`).
#[derive(Clone)]
pub struct MockProvider {
    state: Arc<Mutex<MockState>>,
}

impl MockProvider {
    /// Create a new mock provider with no responses.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                responses: Vec::new(),
                index: 0,
                callback: None,
                error_callback: None,
                history: Vec::new(),
            })),
        }
    }

    /// Provide a fixed sequence of responses.
    ///
    /// Each call to `complete()` (or `stream()`) consumes the next response.
    /// When the sequence is exhausted the provider returns a fallback message.
    pub fn with_responses(self, responses: Vec<Message>) -> Self {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.responses = responses;
        drop(state);
        self
    }

    /// Provide a callback that inspects the full conversation history and
    /// returns the next assistant message.
    ///
    /// The callback receives `&[Message]` (system + user + assistant + tool
    /// messages in order) so you can implement conditional logic such as
    /// "if a tool result exists, return the final answer; otherwise request
    /// the tool call".
    pub fn with_callback<F>(self, callback: F) -> Self
    where
        F: Fn(&[Message]) -> Message + Send + Sync + 'static,
    {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.callback = Some(Box::new(callback));
        drop(state);
        self
    }

    /// Inject an error callback that can fail a call before a response is
    /// resolved.
    ///
    /// The callback inspects the incoming messages; returning `Some(err)`
    /// makes `complete()`/`stream()` fail with that error (the failed call is
    /// NOT recorded in `history()`), while `None` proceeds normally. Useful for
    /// testing retry paths such as the context-length overflow compaction.
    pub fn with_error_callback<F>(self, callback: F) -> Self
    where
        F: Fn(&[Message]) -> Option<SyscityError> + Send + Sync + 'static,
    {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.error_callback = Some(Box::new(callback));
        drop(state);
        self
    }

    /// Return every `CompletionRequest` that has been sent to this provider.
    pub fn history(&self) -> Vec<CompletionRequest> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.history.clone()
    }

    /// Number of times `complete()` or `stream()` has been called.
    pub fn call_count(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.history.len()
    }

    /// Reset the response index back to 0 (for reusing the same provider
    /// across multiple independent tests).
    pub fn reset(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.index = 0;
        state.history.clear();
    }

    /// Fail the call if an error callback is installed and rejects the
    /// request. An injected failure is not recorded in `history()`.
    fn check_error(&self, request: &CompletionRequest) -> crate::Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref cb) = state.error_callback {
            if let Some(err) = cb(&request.messages) {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Resolve the next message to return, advancing the sequence if needed.
    fn resolve_message(&self, request: &CompletionRequest) -> Message {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.history.push(request.clone());

        if let Some(ref cb) = state.callback {
            cb(&request.messages)
        } else if state.index < state.responses.len() {
            let msg = state.responses[state.index].clone();
            state.index += 1;
            msg
        } else {
            // Fallback when no callback and sequence exhausted
            Message::assistant("[mock-provider: no more responses]".to_string())
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

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
        128_000
    }

    async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse> {
        self.check_error(&request)?;
        let message = self.resolve_message(&request);
        Ok(CompletionResponse {
            message,
            model: self.default_model().to_string(),
            usage: Some(Usage {
                prompt_tokens: request
                    .messages
                    .iter()
                    .map(|m| m.content.len() as u32 / 4)
                    .sum(),
                completion_tokens: 0,
                total_tokens: 0,
                ..Default::default()
            }),
            finish_reason: Some("stop".to_string()),
        })
    }

    async fn stream(&self, request: CompletionRequest) -> crate::Result<CompletionStream> {
        self.check_error(&request)?;
        let message = self.resolve_message(&request);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        // Emit the content as a single chunk (or split tool_calls if present)
        if let Err(e) = tx.send(CompletionChunk {
            content: Some(message.content.clone()),
            reasoning_content: message.reasoning_content.clone(),
            tool_calls: message.tool_calls.clone(),
            is_done: false,
            error: None,
            usage: None,
        }) {
            warn!("MockProvider stream: failed to send content chunk: {}", e);
        }

        // Final chunk with usage
        let prompt_tokens: u32 = request
            .messages
            .iter()
            .map(|m| m.content.len() as u32 / 4)
            .sum();
        if let Err(e) = tx.send(CompletionChunk {
            content: None,
            reasoning_content: None,
            tool_calls: None,
            is_done: true,
            error: None,
            usage: Some(Usage {
                prompt_tokens,
                completion_tokens: message.content.len() as u32 / 4,
                total_tokens: prompt_tokens + message.content.len() as u32 / 4,
                ..Default::default()
            }),
        }) {
            warn!("MockProvider stream: failed to send done chunk: {}", e);
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{FunctionCall, Role, ToolCall};

    #[test]
    fn test_sequence_mode() {
        let mock = MockProvider::new()
            .with_responses(vec![Message::assistant("first"), Message::assistant("second")]);

        let req = CompletionRequest::default();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let r1 = rt.block_on(mock.complete(req.clone())).unwrap();
        assert_eq!(r1.message.content, "first");

        let r2 = rt.block_on(mock.complete(req.clone())).unwrap();
        assert_eq!(r2.message.content, "second");

        let r3 = rt.block_on(mock.complete(req)).unwrap();
        assert!(r3.message.content.contains("no more responses"));

        assert_eq!(mock.call_count(), 3);
    }

    #[test]
    fn test_callback_mode() {
        let mock = MockProvider::new().with_callback(|messages| {
            if messages.iter().any(|m| m.role == Role::Tool) {
                Message::assistant("final answer")
            } else {
                Message::assistant("tool call requested").with_tool_calls(vec![ToolCall {
                    id: "c1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "file_read".to_string(),
                        arguments: "{}".to_string(),
                    },
                    index: None,
                    result: None,
                }])
            }
        });

        let rt = tokio::runtime::Runtime::new().unwrap();

        // First turn: no tool result yet → should request tool
        let req1 = CompletionRequest {
            messages: vec![Message::user("read the file")],
            ..Default::default()
        };
        let r1 = rt.block_on(mock.complete(req1)).unwrap();
        assert_eq!(r1.message.content, "tool call requested");
        assert!(r1.message.tool_calls.is_some());

        // Second turn: tool result exists → should return final answer
        let req2 = CompletionRequest {
            messages: vec![
                Message::user("read the file"),
                Message::assistant("tool call requested").with_tool_calls(vec![ToolCall {
                    id: "c1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "file_read".to_string(),
                        arguments: "{}".to_string(),
                    },
                    index: None,
                    result: None,
                }]),
                Message::tool("file contents", "c1"),
            ],
            ..Default::default()
        };
        let r2 = rt.block_on(mock.complete(req2)).unwrap();
        assert_eq!(r2.message.content, "final answer");

        assert_eq!(mock.call_count(), 2);
    }

    #[test]
    fn test_history_records_requests() {
        let mock = MockProvider::new().with_responses(vec![Message::assistant("ok")]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let req = CompletionRequest {
            messages: vec![Message::user("hello")],
            ..Default::default()
        };
        let _ = rt.block_on(mock.complete(req.clone()));

        let history = mock.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].messages[0].content, "hello");
    }

    #[test]
    fn test_error_callback_injects_error_not_recorded() {
        let mock = MockProvider::new()
            .with_responses(vec![Message::assistant("ok")])
            .with_error_callback(|messages| {
                if messages.iter().any(|m| m.content.contains("TOO_LONG")) {
                    Some(SyscityError::ExternalService {
                        source: "Test provider: maximum context length is 2048 tokens".into(),
                        cause: None,
                    })
                } else {
                    None
                }
            });

        let rt = tokio::runtime::Runtime::new().unwrap();

        // A rejected request fails and is not recorded in history.
        let reject = CompletionRequest {
            messages: vec![Message::user("this TOO_LONG request")],
            ..Default::default()
        };
        match rt.block_on(mock.stream(reject)) {
            Err(e) => assert!(e.to_string().contains("maximum context length")),
            Ok(_) => panic!("expected injected error"),
        }
        assert_eq!(mock.call_count(), 0, "injected failures must not enter history");

        // A normal request still resolves.
        let ok_req = CompletionRequest {
            messages: vec![Message::user("hello")],
            ..Default::default()
        };
        assert!(rt.block_on(mock.stream(ok_req)).is_ok());
        assert_eq!(mock.call_count(), 1);
    }

    #[test]
    fn test_reset() {
        let mock = MockProvider::new().with_responses(vec![Message::assistant("a")]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let req = CompletionRequest::default();
        let _ = rt.block_on(mock.complete(req.clone()));
        assert_eq!(mock.call_count(), 1);

        mock.reset();
        assert_eq!(mock.call_count(), 0);

        let r2 = rt.block_on(mock.complete(req)).unwrap();
        assert_eq!(r2.message.content, "a"); // sequence restarted
    }
}
