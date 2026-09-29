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

/// `POST /v1/chat/completions`
///
/// OpenAI-compatible chat completions endpoint. Routes the last user message
/// through the default Syscity agent and returns the result in OpenAI wire
/// format. Supports both streaming (`stream: true` → SSE) and non-streaming.
#[allow(unused_assignments)]
pub async fn openai_chat_completions_handler(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<ModelOverrideQuery>,
    headers: axum::http::HeaderMap,
    Json(mut req): Json<OpenAiChatRequest>,
) -> axum::response::Response {
    use axum::response::sse::{Event as SseEvt, KeepAlive, Sse};

    // Request-level model override: header X-Model takes precedence,
    // then query param ?model=..., then JSON body model field.
    if let Some(header_model) = headers.get("x-model").and_then(|v| v.to_str().ok()) {
        req.model = header_model.to_string();
    } else if let Some(query_model) = query.model {
        req.model = query_model;
    }

    // Extract the last user message.
    let user_message = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    if user_message.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {
                    "message": "No user message provided",
                    "type": "invalid_request_error"
                }
            })),
        )
            .into_response();
    }

    // Grab the default agent handle.
    let handle = {
        let agents = state.agents.agents.read().await;
        match agents.get("default").cloned() {
            Some(h) => h,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({
                        "error": {"message": "No agent available", "type": "server_error"}
                    })),
                )
                    .into_response();
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
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": {"message": format!("Agent error: {}", e), "type": "server_error"}
            })),
        )
            .into_response();
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
                return (
                    StatusCode::REQUEST_TIMEOUT,
                    Json(serde_json::json!({
                        "error": {"message": "Request timed out", "type": "server_error"}
                    })),
                )
                    .into_response();
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
                    content: response_content.unwrap_or_default(),
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
        })
    }

    fn user_msg(content: &str) -> OpenAiMessage {
        OpenAiMessage {
            role: "user".to_string(),
            content: content.to_string(),
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
}
