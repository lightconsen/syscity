//! `ask_user` interactive-question handlers.
//!
//! The gateway lifts `AskQueue` broadcasts onto the WS event bus as
//! `ask.required` / `ask.resolved` (see `ask_forwarder` in lifecycle.rs).
//! This module handles the one client->server command: answering a pending
//! question.

use serde::Deserialize;

use super::*;

/// Payload for `ask.respond`.
#[derive(Debug, Deserialize)]
struct AskRespondParams {
    ask_id: String,
    response: String,
}

/// Answer a pending `ask_user` question, waking the blocked tool.
pub(super) async fn handle_ask_respond(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let params: AskRespondParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if params.response.trim().is_empty() {
        return error_invalid_request(&req.id, "response must not be empty");
    }

    let resolved = state
        .tools
        .ask_queue
        .resolve(&params.ask_id, params.response)
        .await;

    if resolved {
        WsResponse::ok(&req.id, serde_json::json!({ "status": "answered" }))
    } else {
        WsResponse::err(&req.id, "ASK_NOT_FOUND", "Question not found or already answered")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;

    fn make_req(id: &str, method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params: Some(params),
        }
    }

    #[tokio::test]
    async fn test_ask_respond_answers_pending_question() {
        let state = Arc::new(make_test_state(crate::gateway::GatewayConfig::default()).await);
        let queue = state.tools.ask_queue.clone();

        // Submit a pending question directly into the queue.
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let id = queue
            .submit(crate::tools::ask_user::PendingQuestion {
                request: crate::tools::ask_user::AskRequest {
                    id: "ask-1".to_string(),
                    conversation_id: "conv-1".to_string(),
                    question: "Proceed?".to_string(),
                    options: vec![],
                    required: true,
                    default: None,
                },
                response_tx: Some(tx),
            })
            .await;

        let req =
            make_req("r1", "ask.respond", serde_json::json!({ "ask_id": id, "response": "yes" }));
        let res = handle_ask_respond(&req, &state).await;
        assert!(res.ok, "respond failed: {:?}", res.error);
        assert_eq!(res.payload.unwrap().get("status").unwrap(), "answered");
        assert_eq!(rx.try_recv().unwrap(), "yes");
    }

    #[tokio::test]
    async fn test_ask_respond_not_found() {
        let state = Arc::new(make_test_state(crate::gateway::GatewayConfig::default()).await);
        let req = make_req(
            "r2",
            "ask.respond",
            serde_json::json!({ "ask_id": "ask-missing", "response": "x" }),
        );
        let res = handle_ask_respond(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "ASK_NOT_FOUND");
    }

    #[tokio::test]
    async fn test_ask_respond_empty_response_rejected() {
        let state = Arc::new(make_test_state(crate::gateway::GatewayConfig::default()).await);
        let req =
            make_req("r3", "ask.respond", serde_json::json!({ "ask_id": "ask-1", "response": "" }));
        let res = handle_ask_respond(&req, &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }
}
