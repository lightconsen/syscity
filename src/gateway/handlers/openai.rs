use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use tokio::sync::mpsc;
use tracing::warn;

use crate::gateway::GatewayState;
use crate::gateway::*;
use crate::providers::{FunctionCall, Message, Role, ToolCall};

/// `POST /v1/chat/completions`
///
/// OpenAI-compatible chat completions endpoint. Two modes, selected by the
/// request's `mode` field:
///
/// - `agent` (default): routes the last user message through the default
///   Syscity agent and returns the result. Supports streaming (`stream: true`
///   → SSE) and non-streaming.
/// - `raw`: one model turn. The caller's full `messages` history and `tools`
///   are passed straight to the provider, and `tool_calls` come back in the
///   response for the caller to execute. This is the shape external agent
///   benchmarks drive a model with.
pub async fn openai_chat_completions_handler(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<ModelOverrideQuery>,
    headers: axum::http::HeaderMap,
    Json(mut req): Json<OpenAiChatRequest>,
) -> axum::response::Response {
    // Request-level model override: header X-Model takes precedence,
    // then query param ?model=..., then JSON body model field.
    if let Some(header_model) = headers.get("x-model").and_then(|v| v.to_str().ok()) {
        req.model = header_model.to_string();
    } else if let Some(query_model) = query.model {
        req.model = query_model;
    }

    match req.mode {
        OpenAiMode::Raw => raw_completion(state, req).await,
        OpenAiMode::Agent => agent_completion(state, req).await,
    }
}

/// Models that mean "whatever the gateway is configured to use". A benchmark
/// harness has no reason to know the operator's model ids, so it can send one
/// of these and let the gateway resolve it.
const RAW_MODEL_SENTINELS: [&str; 3] = ["syscity-agent", "syscity", "default"];

fn error_body(
    status: StatusCode,
    kind: &str,
    message: impl Into<String>,
) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "error": { "message": message.into(), "type": kind }
        })),
    )
        .into_response()
}

/// One model turn with caller-supplied tools (`mode: "raw"`).
///
/// Deliberately does not touch the agent registry: a benchmark needs a model
/// endpoint, not the built-in assistant, and requiring a running default agent
/// would make this route unusable in exactly the setups that want it.
async fn raw_completion(
    state: Arc<GatewayState>,
    req: OpenAiChatRequest,
) -> axum::response::Response {
    if req.stream {
        return error_body(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "raw mode does not support stream: true",
        );
    }

    let messages = match to_provider_messages(&req.messages) {
        Ok(messages) => messages,
        Err(err) => return error_body(StatusCode::BAD_REQUEST, "invalid_request_error", err),
    };
    if messages.is_empty() {
        return error_body(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "No messages provided",
        );
    }

    let requested_model = req.model.trim().to_string();
    let model = if requested_model.is_empty()
        || RAW_MODEL_SENTINELS
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&requested_model))
    {
        state.infra.model_router.get_default_model().await
    } else {
        requested_model
    };

    let tools = if req.tools.is_empty() {
        None
    } else {
        Some(req.tools)
    };

    // Same bound as the agent path: a provider that never answers must not hold
    // the HTTP request open forever.
    let completion = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        state.infra.model_router.complete(&model, messages, tools),
    )
    .await
    .unwrap_or_else(|_| {
        Err(crate::error::SyscityError::ExternalService {
            source: format!("model '{model}' did not answer within 120s"),
            cause: None,
        })
    });

    let response = match completion {
        Ok(response) => response,
        Err(err) => {
            return error_body(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                format!("provider call failed: {err}"),
            )
        }
    };

    let tool_calls = response
        .message
        .tool_calls
        .clone()
        .filter(|calls| !calls.is_empty())
        .map(to_openai_tool_calls);
    let has_tool_calls = tool_calls.is_some();
    // A turn that only calls tools carries `content: null`, not `""`.
    let content = if response.message.content.is_empty() && has_tool_calls {
        None
    } else {
        Some(response.message.content.clone())
    };
    // Callers key off this to decide whether to execute the tool calls, so it
    // follows the presence of tool calls rather than trusting a provider that
    // reports "stop" for a tool-calling turn.
    let finish_reason = if has_tool_calls {
        "tool_calls".to_string()
    } else {
        response
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string())
    };
    let usage = response.usage.as_ref();
    let resp_model = if response.model.is_empty() {
        model
    } else {
        response.model.clone()
    };

    Json(OpenAiChatResponse {
        id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: resp_model,
        choices: vec![OpenAiChoice {
            index: 0,
            message: OpenAiResponseMessage {
                role: "assistant".to_string(),
                content,
                tool_calls,
            },
            finish_reason,
        }],
        usage: OpenAiUsage {
            prompt_tokens: usage.map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens: usage.map(|u| u.completion_tokens).unwrap_or(0),
            total_tokens: usage.map(|u| u.total_tokens).unwrap_or(0),
        },
    })
    .into_response()
}

/// Map the OpenAI wire messages onto provider messages, keeping tool calls and
/// tool results so a multi-turn conversation survives the round trip.
fn to_provider_messages(messages: &[OpenAiMessage]) -> Result<Vec<Message>, String> {
    messages
        .iter()
        .map(|msg| {
            let role = match msg.role.as_str() {
                "system" | "developer" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" | "function" => Role::Tool,
                other => return Err(format!("unsupported message role '{other}'")),
            };
            let tool_calls = msg.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call.id.clone(),
                        call_type: if call.call_type.is_empty() {
                            "function".to_string()
                        } else {
                            call.call_type.clone()
                        },
                        function: FunctionCall {
                            name: call.function.name.clone(),
                            arguments: call.function.arguments.clone(),
                        },
                        index: None,
                        result: None,
                    })
                    .collect()
            });
            Ok(Message {
                role,
                content: msg.content.clone().unwrap_or_default(),
                content_blocks: None,
                reasoning_content: None,
                name: msg.name.clone(),
                tool_calls,
                tool_call_id: msg.tool_call_id.clone(),
                metadata: None,
            })
        })
        .collect()
}

fn to_openai_tool_calls(calls: Vec<ToolCall>) -> Vec<OpenAiToolCall> {
    calls
        .into_iter()
        .map(|call| OpenAiToolCall {
            id: if call.id.is_empty() {
                format!("call_{}", uuid::Uuid::new_v4().simple())
            } else {
                call.id
            },
            call_type: if call.call_type.is_empty() {
                "function".to_string()
            } else {
                call.call_type
            },
            function: OpenAiFunctionCall {
                name: call.function.name,
                arguments: call.function.arguments,
            },
        })
        .collect()
}

/// The built-in agent path (`mode: "agent"`, the default).
#[allow(unused_assignments)]
async fn agent_completion(
    state: Arc<GatewayState>,
    req: OpenAiChatRequest,
) -> axum::response::Response {
    use axum::response::sse::{Event as SseEvt, KeepAlive, Sse};

    // Extract the last user message.
    let user_message = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
        .unwrap_or_default();

    if user_message.is_empty() {
        return error_body(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "No user message provided",
        );
    }

    // Grab the default agent handle.
    let handle = {
        let agents = state.agents.agents.read().await;
        match agents.get("default").cloned() {
            Some(h) => h,
            None => {
                return error_body(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server_error",
                    "No agent available",
                )
            }
        }
    };

    // Subscribe to events before sending the command to avoid a race.
    let mut event_rx = state.events.tx.subscribe();
    let session_id = uuid::Uuid::new_v4().to_string();

    let cmd = AgentCommand::ProcessMessage {
        session_id: session_id.clone(),
        message: user_message,
        user_id: "openai_api".to_string(),
        channel: "api".to_string(),
        model_override: Some(req.model.clone()),
    };

    if let Err(e) = handle.tx.send(cmd).await {
        return error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            format!("Agent error: {}", e),
        );
    }

    if req.stream {
        // ── Streaming SSE response ──────────────────────────────────────────
        let model = req.model.clone();
        let (tx, rx) = mpsc::channel::<Result<SseEvt, std::convert::Infallible>>(64);
        let sse_session_id = session_id.clone();

        let sse_task = tokio::spawn(async move {
            let completion_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
            let created = chrono::Utc::now().timestamp();
            let timeout_dur = tokio::time::Duration::from_secs(120);
            let start = tokio::time::Instant::now();

            // Wait for the full agent response.
            let response_content = loop {
                if start.elapsed() > timeout_dur {
                    break String::new();
                }
                match tokio::time::timeout(tokio::time::Duration::from_millis(100), event_rx.recv())
                    .await
                {
                    Ok(Ok(GatewayEvent::AgentResponse { session_id: sid, content, .. }))
                        if sid == sse_session_id =>
                    {
                        break content;
                    }
                    Ok(Err(_)) | Err(_) => {}
                    _ => {}
                }
            };

            // Stream the response word-by-word.
            for word in response_content.split_inclusive(|c: char| c.is_whitespace()) {
                let chunk = serde_json::json!({
                    "id": completion_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {"content": word}, "finish_reason": null}]
                });
                if tx
                    .send(Ok(SseEvt::default().data(chunk.to_string())))
                    .await
                    .is_err()
                {
                    break;
                }
            }

            // Final chunk with finish_reason = "stop".
            let final_chunk = serde_json::json!({
                "id": completion_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            });
            if tx
                .send(Ok(SseEvt::default().data(final_chunk.to_string())))
                .await
                .is_ok()
            {
                if let Err(e) = tx.send(Ok(SseEvt::default().data("[DONE]"))).await {
                    warn!("Failed to send SSE [DONE] chunk: {}", e);
                }
            }
        });
        state
            .task_registry
            .insert_join(format!("openai:sse:{}", session_id), sse_task)
            .await;

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response()
    } else {
        // ── Non-streaming JSON response ─────────────────────────────────────
        let timeout_dur = tokio::time::Duration::from_secs(120);
        let start = tokio::time::Instant::now();
        let mut response_content: Option<String> = None;
        let mut prompt_tokens = 0u32;
        let mut completion_tokens = 0u32;
        let mut total_tokens = 0u32;

        loop {
            if start.elapsed() > timeout_dur {
                return error_body(
                    StatusCode::REQUEST_TIMEOUT,
                    "server_error",
                    "Request timed out",
                );
            }

            match tokio::time::timeout(tokio::time::Duration::from_millis(100), event_rx.recv())
                .await
            {
                Ok(Ok(GatewayEvent::AgentResponse {
                    session_id: sid,
                    content,
                    usage,
                    ..
                })) if sid == session_id => {
                    response_content = Some(content);
                    if let Some(ref u) = usage {
                        prompt_tokens = u.prompt_tokens;
                        completion_tokens = u.completion_tokens;
                        total_tokens = u.total_tokens;
                    }
                    break;
                }
                Ok(Err(_)) | Err(_) => {}
                _ => {}
            }
        }

        let resp = OpenAiChatResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp(),
            model: req.model.clone(),
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiResponseMessage {
                    role: "assistant".to_string(),
                    content: Some(response_content.unwrap_or_default()),
                    tool_calls: None,
                },
                finish_reason: "stop".to_string(),
            }],
            usage: OpenAiUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            },
        };

        Json(resp).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    fn req(model: &str, messages: Vec<OpenAiMessage>, stream: bool) -> Json<OpenAiChatRequest> {
        Json(OpenAiChatRequest {
            model: model.to_string(),
            messages,
            stream,
            tools: Vec::new(),
            tool_choice: None,
            mode: OpenAiMode::Agent,
        })
    }

    fn raw_req(model: &str, messages: Vec<OpenAiMessage>) -> Json<OpenAiChatRequest> {
        Json(OpenAiChatRequest {
            model: model.to_string(),
            messages,
            stream: false,
            tools: Vec::new(),
            tool_choice: None,
            mode: OpenAiMode::Raw,
        })
    }

    fn user_msg(content: &str) -> OpenAiMessage {
        OpenAiMessage {
            role: "user".to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    async fn body_json(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn empty_messages_returns_400() {
        let state = state().await;
        let resp = openai_chat_completions_handler(
            State(state),
            Query(ModelOverrideQuery { model: None }),
            HeaderMap::new(),
            req("gpt-4o", vec![], false),
        )
        .await;
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["type"], "invalid_request_error");
        assert_eq!(json["error"]["message"], "No user message provided");
    }

    #[tokio::test]
    async fn no_default_agent_returns_503() {
        let state = state().await;
        let resp = openai_chat_completions_handler(
            State(state),
            Query(ModelOverrideQuery { model: None }),
            HeaderMap::new(),
            req("gpt-4o", vec![user_msg("hello")], false),
        )
        .await;
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["error"]["type"], "server_error");
        assert_eq!(json["error"]["message"], "No agent available");
    }

    #[tokio::test]
    async fn raw_mode_does_not_need_a_default_agent() {
        // The test state has no agents at all. Raw mode must not report "No
        // agent available" — it never touches the registry.
        let state = state().await;
        let resp = openai_chat_completions_handler(
            State(state),
            Query(ModelOverrideQuery { model: None }),
            HeaderMap::new(),
            raw_req("syscity-agent", vec![user_msg("hello")]),
        )
        .await;
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(json["error"]["type"], "upstream_error");
        assert_ne!(json["error"]["message"], "No agent available");
    }

    #[tokio::test]
    async fn raw_mode_rejects_streaming() {
        let state = state().await;
        let mut body = raw_req("syscity-agent", vec![user_msg("hello")]);
        body.0.stream = true;
        let resp = openai_chat_completions_handler(
            State(state),
            Query(ModelOverrideQuery { model: None }),
            HeaderMap::new(),
            body,
        )
        .await;
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn raw_mode_rejects_unknown_roles() {
        let state = state().await;
        let message = OpenAiMessage {
            role: "wizard".to_string(),
            content: Some("hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let resp = openai_chat_completions_handler(
            State(state),
            Query(ModelOverrideQuery { model: None }),
            HeaderMap::new(),
            raw_req("syscity-agent", vec![message]),
        )
        .await;
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported message role 'wizard'"));
    }

    /// End-to-end: a caller-supplied tool list reaches the provider, and the
    /// provider's tool call comes back in OpenAI wire shape. This is the
    /// contract external benchmarks are driven through.
    #[tokio::test]
    async fn raw_mode_passes_tools_through_and_returns_tool_calls() {
        use crate::model_router::ModelCatalogEntry;
        use crate::providers::mock::MockProvider;

        let state = state().await;
        let mock = MockProvider::new().with_responses(vec![Message::assistant("")
            .with_tool_calls(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "get_weather".to_string(),
                    arguments: r#"{"city":"Paris"}"#.to_string(),
                },
                index: None,
                result: None,
            }])]);
        state
            .infra
            .model_router
            .add_provider_instance("mock", Arc::new(mock.clone()))
            .await
            .expect("provider registers");
        state
            .infra
            .model_router
            .model_catalog
            .register(ModelCatalogEntry::new("mock-tool-model", "Mock", "mock").with_tools())
            .await;

        let body: OpenAiChatRequest = serde_json::from_value(serde_json::json!({
            "model": "mock-tool-model",
            "mode": "raw",
            "messages": [
                {"role": "system", "content": "you are a test"},
                {"role": "user", "content": "what is the weather in Paris?"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "description": "Get the weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }}]
        }))
        .expect("request parses");

        let resp = openai_chat_completions_handler(
            State(state),
            Query(ModelOverrideQuery { model: None }),
            HeaderMap::new(),
            Json(body),
        )
        .await;
        let (status, json) = body_json(resp).await;

        assert_eq!(status, StatusCode::OK);
        let message = &json["choices"][0]["message"];
        assert!(message["content"].is_null());
        assert_eq!(message["tool_calls"][0]["type"], "function");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(
            json["choices"][0]["finish_reason"], "tool_calls",
            "callers key off this to decide whether to run the tools"
        );

        // The provider saw the caller's tools and the whole history.
        let calls = mock.history();
        assert_eq!(calls.len(), 1);
        let tools = calls[0].tools.as_ref().expect("tools were passed through");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(calls[0].messages.len(), 2);
        assert_eq!(calls[0].messages[0].role, Role::System);
        assert_eq!(calls[0].messages[1].role, Role::User);
    }

    /// The request side of a tool-using conversation: `content: null` on the
    /// assistant turn and `tool_call_id` on the tool turn must both survive
    /// deserialization, or every turn after the first tool call is lost.
    #[test]
    fn tool_turns_deserialize_from_the_openai_wire() {
        let body = serde_json::json!({
            "model": "syscity-agent",
            "mode": "raw",
            "messages": [
                {"role": "system", "content": "you are a test"},
                {"role": "user", "content": "what is the weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "12C"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "description": "Get the weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }}]
        });

        let req: OpenAiChatRequest = serde_json::from_value(body).expect("request parses");
        assert_eq!(req.mode, OpenAiMode::Raw);
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].function.name, "get_weather");
        assert!(req.messages[2].content.is_none());
        assert_eq!(
            req.messages[2].tool_calls.as_ref().unwrap()[0]
                .function
                .name,
            "get_weather"
        );
        assert_eq!(req.messages[3].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn mode_defaults_to_agent() {
        let req: OpenAiChatRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .expect("request parses");
        assert_eq!(req.mode, OpenAiMode::Agent);
        assert!(req.tools.is_empty());
    }

    #[test]
    fn provider_messages_keep_tool_calls_and_results() {
        let req: OpenAiChatRequest = serde_json::from_value(serde_json::json!({
            "model": "syscity-agent",
            "mode": "raw",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "12C"}
            ]
        }))
        .expect("request parses");

        let messages = to_provider_messages(&req.messages).expect("maps");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::Assistant);
        assert_eq!(messages[0].content, "");
        let calls = messages[0].tool_calls.as_ref().expect("tool calls kept");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].call_type, "function");
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(messages[1].role, Role::Tool);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn response_tool_calls_use_the_openai_type_field() {
        let calls = to_openai_tool_calls(vec![ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: "{\"city\":\"Paris\"}".to_string(),
            },
            index: Some(0),
            result: Some("12C".to_string()),
        }]);
        let resp = OpenAiChatResponse {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "syscity-agent".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiResponseMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(calls),
                },
                finish_reason: "tool_calls".to_string(),
            }],
            usage: OpenAiUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
        };

        let wire = serde_json::to_value(&resp).unwrap();
        let message = &wire["choices"][0]["message"];
        // Tool-only turn: content is null, not "".
        assert!(message["content"].is_null());
        assert_eq!(message["tool_calls"][0]["type"], "function");
        assert_eq!(message["tool_calls"][0]["id"], "call_1");
        // Arguments stay a JSON-encoded string, and harness-only fields are gone.
        assert!(message["tool_calls"][0]["function"]["arguments"].is_string());
        assert!(message["tool_calls"][0].get("call_type").is_none());
        assert!(message["tool_calls"][0].get("result").is_none());
    }

    #[test]
    fn agent_mode_response_omits_tool_calls() {
        let message = OpenAiResponseMessage {
            role: "assistant".to_string(),
            content: Some("hello".to_string()),
            tool_calls: None,
        };
        let wire = serde_json::to_value(&message).unwrap();
        assert_eq!(wire["content"], "hello");
        assert!(wire.get("tool_calls").is_none());
    }
}
