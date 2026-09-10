//! chat.send / chat.history / chat.abort handlers.

use super::*;
pub(super) async fn handle_chat_send(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct ChatSendParams {
        #[serde(alias = "content")]
        message: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
        /// Entity chips attached by the web composer: skills / agents the user
        /// explicitly referenced on this message. Optional; old clients omit it.
        #[serde(default)]
        mentions: Option<MentionsParams>,
    }

    #[derive(Debug, Deserialize, serde::Serialize)]
    struct MentionsParams {
        #[serde(default)]
        skills: Vec<String>,
        #[serde(default)]
        agents: Vec<String>,
    }

    let params: ChatSendParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let (session_id, _is_new_session) = if let Some(sid) = params.session_id {
        (sid, false)
    } else {
        let cg = conn.read().await;
        let channel = cg.client.as_ref().map(|c| c.id.as_str()).unwrap_or("ws");
        let user = cg
            .user_id
            .as_ref()
            .map(|u| u.0.as_str())
            .unwrap_or("anonymous");
        (format!("{}:{}", channel, user), true)
    };

    // Identity comes from the per-request context threaded in by the
    // dispatcher (same value as the handshake-resolved user id).
    let user_id = ctx.user_id().to_string();

    let mut should_name = false;
    if let Some(ref store) = state.agents.store {
        if let Err(e) = store
            .append_message(&AppendMessageParams {
                session_id: &session_id,
                role: "user",
                content: &params.message,
                ..Default::default()
            })
            .await
        {
            tracing::warn!("Failed to save user message to session history: {}", e);
        }
        if let Ok(ps) = store.load_session(&session_id).await {
            if ps.as_ref().map(|m| m.message_count).unwrap_or(0) <= 1 {
                if let Ok(existing) = store.get_session_name(&session_id).await {
                    if existing.is_none() {
                        should_name = true;
                    }
                }
            }
        }
    }

    if should_name {
        let store = state.agents.store.clone();
        let router = state.infra.model_router.clone();
        let events = state.events.tx.clone();
        let sid = session_id.clone();
        let msg = params.message.clone();

        let name_task = tokio::spawn(async move {
            let name = handshake::generate_session_title(&router, &msg)
                .await
                .unwrap_or_else(|e| {
                    tracing::debug!("LLM session title generation failed: {}, using fallback", e);
                    handshake::fallback_session_name(&msg)
                });

            if let Some(ref s) = store {
                if let Err(e) = s.set_session_name(&sid, &name).await {
                    tracing::warn!("Failed to save session name for {}: {}", sid, e);
                } else {
                    tracing::info!("Session {} named: '{}'", sid, name);
                    if let Err(e) = events.send(GatewayEvent::SessionRenamed {
                        session_id: sid.clone(),
                        name: name.clone(),
                    }) {
                        tracing::debug!("No receivers for SessionRenamed event: {}", e);
                    }
                }
            }
        });
        state
            .task_registry
            .insert_join(format!("ws:session_name:{}", session_id), name_task)
            .await;
    }

    if let Some(ref store) = state.agents.store {
        if let Ok(Some(ps)) = store.load_session(&session_id).await {
            if let Some(ref bound_agent) = ps.metadata.bound_agent_id {
                let route = crate::inbound::RouteResult {
                    agent_id: bound_agent.clone(),
                    workspace_id: None,
                    persisted_binding: false,
                    is_fallback: false,
                };
                state.agents.router.bind_session(&session_id, &route).await;
            }
        }
    }

    if let Some(agent_id) = params.agent_id {
        let route = crate::inbound::RouteResult {
            agent_id,
            workspace_id: None,
            persisted_binding: false,
            is_fallback: false,
        };
        state.agents.router.bind_session(&session_id, &route).await;
    }

    // ── Smart name-based routing: "小王，xxx" -> route to secretary-xiaowang ──
    let mut final_message = params.message.clone();
    {
        let registry = state.agents.registry.read().await;
        // Try to extract a name prefix like "小王，" or "小王：" from the message.
        let trimmed = final_message.trim_start();
        if let Some((first_word, rest)) = trimmed.split_once(['，', ',', '：', ':', ' ', '\t']) {
            let name = first_word.trim();
            if !name.is_empty() {
                if let Some((personality, _matched_alias)) = registry.find_by_alias(name) {
                    let agent_id = personality.id.clone();
                    info!(
                        "Smart-routing session {} to agent '{}' (matched name: '{}' in message)",
                        session_id, agent_id, name
                    );
                    let route = crate::inbound::RouteResult {
                        agent_id: agent_id.clone(),
                        workspace_id: None,
                        persisted_binding: true,
                        is_fallback: false,
                    };
                    state.agents.router.bind_session(&session_id, &route).await;
                    // Strip the greeting prefix so the agent sees only the task.
                    final_message = rest.trim_start().to_string();
                }
            }
        }
    }

    let mut incoming =
        crate::channels::IncomingMessage::new(user_id.clone(), session_id.clone(), final_message)
            .with_provenance(crate::channels::InputProvenance::ExternalUser {
                channel: "web".to_string(),
                is_direct: true,
            });

    // Attach composer chips after the smart-routing block: mentions describe
    // the user's intent regardless of any greeting prefix stripped above.
    if let Some(m) = params
        .mentions
        .filter(|m| !m.skills.is_empty() || !m.agents.is_empty())
    {
        if let Ok(v) = serde_json::to_value(&m) {
            incoming.metadata.extra.insert("mentions".to_string(), v);
        }
    }

    // Submit to the unified inbound entry channel instead of calling the
    // pipeline directly. The worker drives the message through the pipeline
    // and `process_routed_messages` dispatches to the agent.
    let routed = match state.pipelines.inbound_entry.send(incoming).await {
        Ok(()) => {
            // Best-effort synchronous resolution so the WebSocket client gets
            // immediate feedback. The resolved agent is also what will receive
            // the message via `routed_tx`.
            Some(state.agents.router.resolve_by_session(&session_id).await)
        }
        Err(e) => {
            return WsResponse::err(
                &req.id,
                "enqueue_failed",
                format!("Failed to enqueue message: {}", e),
            );
        }
    };

    let is_new_session = {
        let mut cg = conn.write().await;
        let is_new = !cg.subscriptions.contains(&session_id);
        if is_new {
            cg.subscriptions.push(session_id.clone());
        }
        is_new
    };

    if is_new_session {
        if let Err(e) = state
            .events
            .tx
            .send(crate::gateway::GatewayEvent::SessionCreated {
                session_id: session_id.clone(),
                agent_id: routed
                    .as_ref()
                    .map(|r| r.agent_id.clone())
                    .unwrap_or_default(),
                user_id: user_id.clone(),
            })
        {
            warn!("Failed to broadcast SessionCreated for {}: {}", session_id, e);
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "accepted",
            "session_id": session_id,
            "agent_id": routed.map(|r| r.agent_id).unwrap_or_default(),
        }),
    )
}

pub(super) async fn handle_chat_history(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct HistoryParams {
        session_id: String,
        #[serde(default = "default_limit")]
        limit: usize,
        /// Optional timestamp in milliseconds. If provided, only messages with
        /// `created_at` strictly less than this value are returned (older
        /// messages).
        #[serde(default)]
        before: Option<i64>,
    }

    fn default_limit() -> usize {
        100
    }

    let params: HistoryParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let before = params.before.and_then(DateTime::from_timestamp_millis);

    let messages = if let Some(ref store) = state.agents.store {
        match store
            .get_messages(&params.session_id, params.limit as i64, before)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .map(
                    |(
                        id,
                        role,
                        content,
                        reasoning,
                        tool_calls_json,
                        dt,
                        _transcript_id,
                        _run_id,
                        turn_id,
                    )| {
                        let tool_calls: Option<serde_json::Value> =
                            tool_calls_json.and_then(|json| serde_json::from_str(&json).ok());
                        serde_json::json!({
                            "id": format!("msg_{}", id),
                            "role": role,
                            "content": content,
                            "reasoning_content": reasoning,
                            "tool_calls": tool_calls,
                            "timestamp": dt.timestamp_millis(),
                            "turn_id": turn_id,
                        })
                    },
                )
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": params.session_id,
            "messages": messages,
            "has_more": messages.len() == params.limit,
        }),
    )
}

pub(super) async fn handle_chat_abort(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct AbortParams {
        session_id: String,
    }

    let params: AbortParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = state.agents.acp.cancel(params.session_id.clone()).await {
        warn!("Failed to cancel session {} during abort: {}", params.session_id, e);
    }
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "aborted",
            "session_id": params.session_id,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::{
        make_test_conn, make_test_state, make_test_state_parts, make_test_state_with_store,
    };
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

    fn ctx() -> RequestContext {
        RequestContext::anonymous()
    }

    #[tokio::test]
    async fn chat_send_missing_params_errors() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_chat_send(&req("r1", None), &conn, &state, &ctx()).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn chat_send_enqueue_fails_without_receiver() {
        // Test state's inbound_entry channel has no receiver, so the send
        // path reports an enqueue failure.
        let state = state().await;
        let conn = make_test_conn(&[]);
        let params = Some(serde_json::json!({ "message": "hello", "session_id": "s1" }));
        let resp = handle_chat_send(&req("r1", params), &conn, &state, &ctx()).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "enqueue_failed");
    }

    #[tokio::test]
    async fn chat_send_with_mentions_attaches_metadata() {
        let (state, mut entry_rx) = make_test_state_parts(GatewayConfig::default()).await;
        let state = Arc::new(state);
        let conn = make_test_conn(&[]);
        let params = Some(serde_json::json!({
            "message": "hello",
            "session_id": "s1",
            "mentions": { "skills": ["canvas-design"], "agents": ["secretary-xiaowang"] }
        }));
        let resp = handle_chat_send(&req("r1", params), &conn, &state, &ctx()).await;
        assert!(resp.ok, "expected ok, got {:?}", resp.error);

        let incoming = entry_rx.recv().await.expect("enqueued message");
        assert_eq!(incoming.content, "hello");
        let mentions = incoming
            .metadata
            .extra
            .get("mentions")
            .expect("mentions key");
        assert_eq!(mentions["skills"][0], "canvas-design");
        assert_eq!(mentions["agents"][0], "secretary-xiaowang");
    }

    #[tokio::test]
    async fn chat_send_without_mentions_has_no_key() {
        let (state, mut entry_rx) = make_test_state_parts(GatewayConfig::default()).await;
        let state = Arc::new(state);
        let conn = make_test_conn(&[]);
        let params = Some(serde_json::json!({ "message": "plain", "session_id": "s1" }));
        let resp = handle_chat_send(&req("r1", params), &conn, &state, &ctx()).await;
        assert!(resp.ok, "expected ok, got {:?}", resp.error);

        let incoming = entry_rx.recv().await.expect("enqueued message");
        assert_eq!(incoming.content, "plain");
        assert!(
            !incoming.metadata.extra.contains_key("mentions"),
            "no mentions key expected, got {:?}",
            incoming.metadata.extra
        );
    }

    #[tokio::test]
    async fn chat_history_missing_params_errors() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_chat_history(&req("r1", None), &conn, &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn chat_history_empty_without_store() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let params = Some(serde_json::json!({ "session_id": "s1" }));
        let resp = handle_chat_history(&req("r1", params), &conn, &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["session_id"], "s1");
        assert!(payload["messages"].as_array().unwrap().is_empty());
        assert_eq!(payload["has_more"], false);
    }

    #[tokio::test]
    async fn chat_history_empty_with_store() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&[]);
        let params = Some(serde_json::json!({ "session_id": "s1" }));
        let resp = handle_chat_history(&req("r1", params), &conn, &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert!(payload["messages"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn chat_abort_missing_params_errors() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_chat_abort(&req("r1", None), &conn, &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn chat_abort_unknown_session_ok() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let params = Some(serde_json::json!({ "session_id": "ghost" }));
        let resp = handle_chat_abort(&req("r1", params), &conn, &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["status"], "aborted");
        assert_eq!(payload["session_id"], "ghost");
    }
}
