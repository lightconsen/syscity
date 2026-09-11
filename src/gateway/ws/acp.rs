//! ACP (sub-agent) control-plane handlers.

use super::*;
pub(super) async fn handle_acp_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let subagents = state.agents.acp.list_subagents().await;
    let sessions: Vec<_> = subagents
        .iter()
        .map(|s| {
            serde_json::json!({
                "subagent_id": s.id,
                "session_id": s.session_id.to_string(),
                "parent_id": s.parent_id,
                "mode": format!("{:?}", s.mode),
                "status": format!("{:?}", s.status),
                "thread_id": s.thread_id,
            })
        })
        .collect();

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "sessions": sessions,
            "count": sessions.len(),
        }),
    )
}

pub(super) async fn handle_acp_spawn(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    use crate::acp::{AcpSessionId, SpawnMode, SubagentConfig, ThreadBinding};
    use crate::channels::IncomingMessage;
    use crate::security::runtime_audit::AuditEventType;
    use crate::security::RateLimitResult;

    #[derive(Debug, Deserialize)]
    struct SpawnParams {
        task: String,
        #[serde(default = "default_acp_mode")]
        mode: String,
        #[serde(default)]
        agent_type: String,
    }

    fn default_acp_mode() -> String {
        "run".to_string()
    }

    let params: SpawnParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // Identity comes from the per-request context (same value the handshake
    // resolved for this connection).
    let actor = ctx.user_id().to_string();

    let rate_result = state
        .auth
        .rate_limiter
        .check_scoped_context(ctx, "acp:spawn", 1.0)
        .await;
    if !rate_result.is_allowed() {
        let retry = match rate_result {
            RateLimitResult::Denied { retry_after_secs } => retry_after_secs,
            _ => 60,
        };
        return WsResponse::err(
            &req.id,
            "RATE_LIMITED",
            format!("Rate limit exceeded for ACP spawn. Retry after {}s", retry),
        );
    }

    let session_id = AcpSessionId::new();
    let parent_id = actor.clone();

    let mode = match params.mode.as_str() {
        "session" => SpawnMode::Session,
        _ => SpawnMode::Run,
    };

    let agent_type = if params.agent_type.is_empty() {
        "default".to_string()
    } else {
        params.agent_type.clone()
    };
    let config = SubagentConfig {
        mode,
        thread_binding: ThreadBinding::Auto,
        system_prompt: None,
        timeout_seconds: Some(300),
    };

    match state
        .agents
        .acp
        .spawn_subagent(session_id.clone(), parent_id.clone(), config)
        .await
    {
        Ok(handle) => {
            let subagent_id = handle.id.clone();

            state
                .auth
                .audit_log
                .log_with_context(
                    AuditEventType::AcpSpawn,
                    ctx,
                    &subagent_id,
                    true,
                    format!("Spawned subagent via WebSocket (mode: {:?})", handle.mode),
                    Some(serde_json::json!({
                        "session_id": session_id.to_string(),
                        "parent_id": parent_id,
                        "agent_type": agent_type,
                    })),
                )
                .await;

            let message = IncomingMessage::new(actor.clone(), session_id.to_string(), params.task);

            match state.agents.acp.send_message(&subagent_id, message).await {
                Ok(response) => WsResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "subagent_id": subagent_id,
                        "session_id": session_id.to_string(),
                        "mode": format!("{:?}", handle.mode),
                        "response": response,
                    }),
                ),
                Err(e) => {
                    if let Err(shutdown_err) =
                        state.agents.acp.shutdown_subagent(&subagent_id).await
                    {
                        warn!(
                            "Failed to shutdown subagent {} after task failure: {}",
                            subagent_id, shutdown_err
                        );
                    }
                    WsResponse::err(
                        &req.id,
                        "SPAWN_FAILED",
                        format!("Subagent failed to process task: {}", e),
                    )
                }
            }
        }
        Err(e) => {
            state
                .auth
                .audit_log
                .log_with_context(
                    AuditEventType::AcpSpawn,
                    ctx,
                    "",
                    false,
                    format!("Failed to spawn subagent: {}", e),
                    None,
                )
                .await;
            WsResponse::err(&req.id, "SPAWN_FAILED", format!("Failed to spawn subagent: {}", e))
        }
    }
}

pub(super) async fn handle_acp_terminate(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    use crate::acp::AcpSessionId;
    use crate::security::runtime_audit::AuditEventType;

    #[derive(Debug, Deserialize)]
    struct TerminateParams {
        session_id: String,
    }

    let params: TerminateParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let session_id = AcpSessionId(params.session_id.clone());
    match state.agents.acp.terminate_session(&session_id).await {
        Ok(count) => {
            state
                .auth
                .audit_log
                .log_with_context(
                    AuditEventType::AcpTerminate,
                    ctx,
                    &params.session_id,
                    true,
                    format!("Terminated {} subagents in session {}", count, params.session_id),
                    Some(serde_json::json!({ "terminated_count": count })),
                )
                .await;
            WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "terminated_count": count,
                    "session_id": params.session_id,
                }),
            )
        }
        Err(e) => {
            state
                .auth
                .audit_log
                .log_with_context(
                    AuditEventType::AcpTerminate,
                    ctx,
                    &params.session_id,
                    false,
                    format!("Failed to terminate session: {}", e),
                    None,
                )
                .await;
            WsResponse::err(
                &req.id,
                "TERMINATE_FAILED",
                format!("Failed to terminate session: {}", e),
            )
        }
    }
}

pub(super) async fn handle_acp_message(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    use crate::acp::AcpSessionId;
    use crate::channels::IncomingMessage;
    use crate::security::runtime_audit::AuditEventType;

    #[derive(Debug, Deserialize)]
    struct MessageParams {
        session_id: String,
        message: String,
    }

    let params: MessageParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let session_id = AcpSessionId(params.session_id.clone());
    let subagents = state.agents.acp.list_session_subagents(&session_id).await;

    if subagents.is_empty() {
        return WsResponse::err(&req.id, "NO_ACTIVE_SUBAGENTS", "No active subagents in session");
    }

    let subagent = &subagents[0];
    let message =
        IncomingMessage::new(ctx.user_id().to_string(), session_id.to_string(), params.message);

    match state.agents.acp.send_message(&subagent.id, message).await {
        Ok(response) => {
            state
                .auth
                .audit_log
                .log_with_context(
                    AuditEventType::AcpMessage,
                    ctx,
                    &params.session_id,
                    true,
                    format!(
                        "Message sent to subagent {} in session {}",
                        subagent.id, params.session_id
                    ),
                    Some(serde_json::json!({
                        "subagent_id": subagent.id,
                        "session_id": params.session_id,
                    })),
                )
                .await;
            WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "subagent_id": subagent.id,
                    "session_id": session_id.to_string(),
                    "response": response,
                }),
            )
        }
        Err(e) => {
            state
                .auth
                .audit_log
                .log_with_context(
                    AuditEventType::AcpMessage,
                    ctx,
                    &params.session_id,
                    false,
                    format!("Failed to send message: {}", e),
                    None,
                )
                .await;
            WsResponse::err(&req.id, "MESSAGE_FAILED", format!("Failed to send message: {}", e))
        }
    }
}

pub(super) async fn handle_acp_status(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct StatusParams {
        session_id: String,
    }

    let params: StatusParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    match state.agents.acp.get_status(params.session_id.clone()).await {
        Ok(Some(status)) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": status.session_id,
                "runtime_state": format!("{}", status.runtime_state),
                "mode": format!("{:?}", status.mode),
                "current_iteration": status.current_iteration,
                "max_iterations": status.max_iterations,
            }),
        ),
        Ok(None) => WsResponse::err(&req.id, "SESSION_NOT_FOUND", "Session not found"),
        Err(e) => WsResponse::err(&req.id, "ACP_ERROR", format!("Failed to get status: {}", e)),
    }
}

pub(super) async fn handle_acp_pause(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ControlParams {
        session_id: String,
    }

    let params: ControlParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    match state.agents.acp.pause(params.session_id.clone()).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": params.session_id,
                "action": "pause",
                "status": "requested",
            }),
        ),
        Err(e) => WsResponse::err(
            &req.id,
            "PAUSE_FAILED",
            format!("Failed to pause session {}: {}", params.session_id, e),
        ),
    }
}

pub(super) async fn handle_acp_resume(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ControlParams {
        session_id: String,
    }

    let params: ControlParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    match state.agents.acp.resume(params.session_id.clone()).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": params.session_id,
                "action": "resume",
                "status": "requested",
            }),
        ),
        Err(e) => WsResponse::err(
            &req.id,
            "RESUME_FAILED",
            format!("Failed to resume session {}: {}", params.session_id, e),
        ),
    }
}

pub(super) async fn handle_acp_step(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ControlParams {
        session_id: String,
    }

    let params: ControlParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    match state.agents.acp.step(params.session_id.clone()).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": params.session_id,
                "action": "step",
                "status": "requested",
            }),
        ),
        Err(e) => WsResponse::err(
            &req.id,
            "STEP_FAILED",
            format!("Failed to step session {}: {}", params.session_id, e),
        ),
    }
}

pub(super) async fn handle_acp_cancel(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ControlParams {
        session_id: String,
    }

    let params: ControlParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    match state.agents.acp.cancel(params.session_id.clone()).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": params.session_id,
                "action": "cancel",
                "status": "requested",
            }),
        ),
        Err(e) => WsResponse::err(
            &req.id,
            "CANCEL_FAILED",
            format!("Failed to cancel session {}: {}", params.session_id, e),
        ),
    }
}

pub(super) async fn handle_acp_tree(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    use crate::acp::AcpSessionId;

    #[derive(Debug, Deserialize)]
    struct TreeParams {
        session_id: String,
    }

    let params: TreeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let session_id = AcpSessionId(params.session_id.clone());
    let tree = state.agents.acp.get_subagent_tree(&session_id).await;

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": params.session_id,
            "tree": tree,
        }),
    )
}

pub(super) async fn handle_acp_execute_session(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ExecuteParams {
        message: String,
        user_id: String,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        max_iterations: Option<usize>,
    }

    let params: ExecuteParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let agent_id = params.agent_id.unwrap_or_else(|| "default".to_string());
    let agents = state.agents.agents.read().await;
    let agent_handle = match agents.get(&agent_id) {
        Some(h) => h.clone(),
        None => {
            return WsResponse::err(
                &req.id,
                "AGENT_NOT_FOUND",
                format!("Agent '{}' not found", agent_id),
            );
        }
    };
    drop(agents);

    let session_id = uuid::Uuid::new_v4().to_string();

    // Attach structured tracing context for this request.
    // The _guard is explicitly dropped before .await because Entered is !Send.
    let ctx = TracingContext::new(Some(session_id.clone()), Some(params.user_id.clone()));
    let span = ctx.attach_to_span();
    let _guard = span.clone().entered();

    let incoming = crate::channels::IncomingMessage::new(
        params.user_id.clone(),
        session_id.clone(),
        params.message,
    );
    drop(_guard);

    match state
        .agents
        .acp
        .execute_session_with_max_iterations(agent_handle.agent, incoming, params.max_iterations)
        .instrument(span)
        .await
    {
        Ok(outgoing) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "mode": "session",
                "response": outgoing.content,
                "usage": outgoing.usage,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "EXECUTE_FAILED", format!("Execution failed: {}", e)),
    }
}

pub(super) async fn handle_acp_execute_run(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ExecuteParams {
        message: String,
        user_id: String,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        max_iterations: Option<usize>,
    }

    let params: ExecuteParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let agent_id = params.agent_id.unwrap_or_else(|| "default".to_string());
    let agents = state.agents.agents.read().await;
    let agent_handle = match agents.get(&agent_id) {
        Some(h) => h.clone(),
        None => {
            return WsResponse::err(
                &req.id,
                "AGENT_NOT_FOUND",
                format!("Agent '{}' not found", agent_id),
            );
        }
    };
    drop(agents);

    let session_id = uuid::Uuid::new_v4().to_string();

    // Attach structured tracing context for this request.
    // The _guard is explicitly dropped before .await because Entered is !Send.
    let ctx = TracingContext::new(Some(session_id.clone()), Some(params.user_id.clone()));
    let span = ctx.attach_to_span();
    let _guard = span.clone().entered();

    let incoming = crate::channels::IncomingMessage::new(
        params.user_id.clone(),
        session_id.clone(),
        params.message,
    );
    drop(_guard);

    match state
        .agents
        .acp
        .execute_run_with_max_iterations(agent_handle.agent, incoming, params.max_iterations)
        .instrument(span)
        .await
    {
        Ok(outgoing) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "mode": "run",
                "response": outgoing.content,
                "usage": outgoing.usage,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "EXECUTE_FAILED", format!("Execution failed: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    fn req(id: &str, params: Option<serde_json::Value>) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: "x".into(),
            params,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    fn session_params(session_id: &str) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "session_id": session_id }))
    }

    fn ctx() -> RequestContext {
        RequestContext::anonymous()
    }

    #[tokio::test]
    async fn acp_list_empty_ok() {
        let state = state().await;
        let resp = handle_acp_list(&req("r1", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 0);
        assert!(payload["sessions"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn acp_spawn_missing_params_errors() {
        let state = state().await;
        let resp = handle_acp_spawn(&req("r1", None), &state, &ctx()).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn acp_terminate_unknown_session_fails() {
        let state = state().await;
        let resp = handle_acp_terminate(&req("r1", session_params("ghost")), &state, &ctx()).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "TERMINATE_FAILED");
    }

    /// The audit entry a WS handler writes records the *request context's*
    /// actor — not a re-derived or hardcoded literal (docs/scale.md §8, T2).
    #[tokio::test]
    async fn audit_actor_comes_from_request_context() {
        let state = state().await;
        let ctx = RequestContext::new("alice", AuthSource::SharedToken);

        let resp = handle_acp_terminate(&req("r1", session_params("ghost")), &state, &ctx).await;
        assert!(!resp.ok);

        let entries = state.auth.audit_log.recent(1).await;
        assert_eq!(entries.len(), 1, "the failure path must still audit");
        assert_eq!(entries[0].actor, "alice");
        assert_ne!(entries[0].actor, "anonymous");
    }

    #[tokio::test]
    async fn acp_message_missing_params_errors() {
        let state = state().await;
        let resp = handle_acp_message(&req("r1", None), &state, &ctx()).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn acp_message_no_active_subagents() {
        let state = state().await;
        let params = Some(serde_json::json!({ "session_id": "ghost", "message": "hi" }));
        let resp = handle_acp_message(&req("r1", params), &state, &ctx()).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NO_ACTIVE_SUBAGENTS");
    }

    #[tokio::test]
    async fn acp_status_missing_params_errors() {
        let state = state().await;
        let resp = handle_acp_status(&req("r1", None), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn acp_status_unknown_session_not_found() {
        let state = state().await;
        let resp = handle_acp_status(&req("r1", session_params("ghost")), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn acp_pause_unknown_session_requested() {
        let state = state().await;
        let resp = handle_acp_pause(&req("r1", session_params("ghost")), &state).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "requested");
    }

    #[tokio::test]
    async fn acp_resume_unknown_session_requested() {
        let state = state().await;
        let resp = handle_acp_resume(&req("r1", session_params("ghost")), &state).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "requested");
    }

    #[tokio::test]
    async fn acp_step_unknown_session_requested() {
        let state = state().await;
        let resp = handle_acp_step(&req("r1", session_params("ghost")), &state).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "requested");
    }

    #[tokio::test]
    async fn acp_cancel_unknown_session_requested() {
        let state = state().await;
        let resp = handle_acp_cancel(&req("r1", session_params("ghost")), &state).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "requested");
    }

    #[tokio::test]
    async fn acp_tree_unknown_session_empty() {
        let state = state().await;
        let resp = handle_acp_tree(&req("r1", session_params("ghost")), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["session_id"], "ghost");
        assert!(payload["tree"].is_array());
    }

    #[tokio::test]
    async fn acp_execute_session_missing_params_errors() {
        let state = state().await;
        let resp = handle_acp_execute_session(&req("r1", None), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn acp_execute_session_no_default_agent() {
        let state = state().await;
        let params = Some(serde_json::json!({ "message": "hi", "user_id": "u1" }));
        let resp = handle_acp_execute_session(&req("r1", params), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "AGENT_NOT_FOUND");
    }

    #[tokio::test]
    async fn acp_execute_run_no_default_agent() {
        let state = state().await;
        let params = Some(serde_json::json!({ "message": "hi", "user_id": "u1" }));
        let resp = handle_acp_execute_run(&req("r1", params), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "AGENT_NOT_FOUND");
    }
}
