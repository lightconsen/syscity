//! WebSocket connection lifecycle: auth middleware, upgrade validation,
//! the per-connection event loop, and the method dispatcher.

use super::*;

/// Middleware: validate WebSocket upgrade credentials before proceeding.
///
/// Runs BEFORE the WebSocket upgrade. When auth_mode is not "none", rejects
/// with 401 if no valid session cookie, shared token, or query token is found.
pub async fn ws_auth_middleware(
    State(state): State<Arc<GatewayState>>,
    mut req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    // Check auth_mode — if "none", allow anonymous connections
    let auth_mode = {
        let config = state.config.read().await;
        config.security.auth_mode
    };

    if matches!(auth_mode, crate::gateway::protocol::AuthMode::None) {
        // Allow anonymous access
        req.extensions_mut().insert(WsAuthResult {
            user_id: UserId::new("anonymous"),
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
        });
        return next.run(req).await;
    }

    // Extract optional token from query parameter
    let query_token = req.uri().query().and_then(|q| {
        q.split('&')
            .find(|p| p.starts_with("token="))
            .and_then(|p| urlencoding::decode(&p["token=".len()..]).ok())
            .map(|s| s.to_string())
    });

    let auth_result =
        validate_ws_upgrade_request(&state, req.headers(), query_token.as_deref()).await;
    match auth_result {
        Ok(result) => {
            req.extensions_mut().insert(result);
            next.run(req).await
        }
        Err(resp) => resp,
    }
}

/// Validate WebSocket upgrade request credentials BEFORE the handshake.
async fn validate_ws_upgrade_request(
    state: &Arc<GatewayState>,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> Result<WsAuthResult, axum::response::Response> {
    // 1. Try Bearer token from Authorization header
    let token_from_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer ").map(String::from));

    // Check against auth_manager (for Bearer session tokens)
    if let Some(ref tok) = token_from_header {
        if let Some(session) = state.auth.manager.validate_session(tok).await {
            let scopes = if session.scopes.is_empty() {
                DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect()
            } else {
                session.scopes.clone()
            };
            return Ok(WsAuthResult {
                user_id: session.user_id,
                scopes,
            });
        }
    }

    // 3. Check against shared_token in config
    let config = state.config.read().await;
    if let Some(shared_token) = &config.security.shared_token {
        // Check Bearer header token
        if let Some(ref tok) = token_from_header {
            if tok == shared_token {
                return Ok(WsAuthResult {
                    user_id: UserId::new("shared"),
                    scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
                });
            }
        }
        // Check query parameter token
        if let Some(qt) = query_token {
            if qt == shared_token {
                return Ok(WsAuthResult {
                    user_id: UserId::new("shared"),
                    scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
                });
            }
        }
    }

    warn!("WebSocket upgrade rejected: no valid credentials");
    let resp = axum::http::Response::builder()
        .status(axum::http::StatusCode::UNAUTHORIZED)
        .header(axum::http::header::WWW_AUTHENTICATE, "Bearer, Cookie")
        .body(axum::body::Body::from(
            "Unauthorized: valid session cookie or API token required",
        ))
        .unwrap_or_else(|_| {
            axum::http::Response::new(axum::body::Body::from(
                "Unauthorized: valid session cookie or API token required",
            ))
        });
    Err(resp)
}

/// Handler: WebSocket upgrade.
///
/// Credentials are validated by the `ws_auth_middleware` BEFORE this handler
/// is reached. If we get here, auth is already verified.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<WsConnectQuery>,
    axum::Extension(auth_result): axum::Extension<WsAuthResult>,
) -> impl IntoResponse {
    let auth_mode = {
        let config = state.config.read().await;
        config.security.auth_mode
    };

    ws.on_upgrade(move |socket| handle_websocket(socket, state, query, auth_mode, auth_result))
}

async fn handle_websocket(
    socket: WebSocket,
    state: Arc<GatewayState>,
    query: WsConnectQuery,
    auth_mode: crate::gateway::protocol::AuthMode,
    auth_result: WsAuthResult,
) {
    let conn_id = Uuid::new_v4().to_string();
    info!("[{}] WebSocket connected", conn_id);

    let mut proto_conn = ProtocolConnection::new(conn_id.clone());
    if let Some(sid) = query.session_id {
        proto_conn.subscriptions.push(sid);
    }
    let conn = Arc::new(tokio::sync::RwLock::new(proto_conn));

    let mut event_rx = state.events.tx.subscribe();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<WsCommand>(256);

    let (mut ws_sender, mut ws_receiver): (SplitSink<WebSocket, Message>, SplitStream<WebSocket>) =
        StreamExt::split(socket);
    let conn_send = conn.clone();

    let conn_task_prefix = format!("ws:conn:{}", conn_id);
    let task_registry = state.task_registry.clone();

    let send_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok(event) = event_rx.recv() => {
                    let conn_guard = conn_send.read().await;
                    if !conn_guard.handshaked {
                        continue;
                    }

                    let should_send = match &event {
                        GatewayEvent::AgentResponse { session_id, .. }
                        | GatewayEvent::ToolCalling { session_id, .. }
                        | GatewayEvent::ToolResult { session_id, .. }
                        | GatewayEvent::Completed { session_id, .. }
                        | GatewayEvent::ProcessingError { session_id, .. }
                        | GatewayEvent::Thinking { session_id, .. }
                        | GatewayEvent::GoalProgress { session_id, .. }
                        | GatewayEvent::AskRequired(crate::tools::ask_user::AskRequiredEvent {
                            session_id,
                            ..
                        }) => {
                            conn_guard.is_subscribed(session_id)
                        }
                        GatewayEvent::AskResolved(e) => {
                            // Route resolution to whoever is subscribed to the
                            // session the question belonged to (best-effort:
                            // the ask_id may resolve across subscribers).
                            conn_guard.is_subscribed(&e.session_id)
                        }
                        _ => true,
                    };

                    if !should_send {
                        continue;
                    }
                    drop(conn_guard);

                    if let Some((event_name, payload)) = gateway_event_to_ws(&event) {
                        let seq = {
                            let mut cg = conn_send.write().await;
                            cg.next_seq()
                        };
                        let ws_event = WsEvent::new(event_name, payload, seq);
                        if let Ok(text) = serde_json::to_string(&ws_event) {
                            if ws_sender.send(Message::Text(text)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        WsCommand::SendResponse(text) | WsCommand::SendEvent(text) => {
                            if ws_sender.send(Message::Text(text)).await.is_err() {
                                break;
                            }
                        }
                        WsCommand::Subscribe(ids) => {
                            let mut cg = conn_send.write().await;
                            for id in ids {
                                if !cg.subscriptions.contains(&id) {
                                    cg.subscriptions.push(id);
                                }
                            }
                        }
                        WsCommand::Unsubscribe(ids) => {
                            let mut cg = conn_send.write().await;
                            cg.subscriptions.retain(|s| !ids.contains(s));
                        }
                    }
                }
                else => break,
            }
        }
    });
    task_registry
        .insert_join(format!("{}:send", conn_task_prefix), send_task)
        .await;

    let recv_task = tokio::spawn(async move {
        let handshake_ok = loop {
            let msg = ws_receiver.next().await;

            match msg {
                Some(Ok(Message::Text(text))) => {
                    let conn_id = conn.read().await.conn_id.clone();
                    debug!("[{}] Received: {}", conn_id, text);

                    match serde_json::from_str::<WsRequest>(&text) {
                        Ok(req) => {
                            if req.method == "connect" {
                                let res = handshake::handle_connect(
                                    &req,
                                    &conn,
                                    &state,
                                    &auth_mode,
                                    &cmd_tx,
                                    &auth_result,
                                )
                                .await;
                                let res_text = serde_json::to_string(&res).unwrap_or_default();
                                if cmd_tx
                                    .send(WsCommand::SendResponse(res_text))
                                    .await
                                    .is_err()
                                {
                                    warn!("[{}] Failed to send handshake response", conn_id);
                                    break false;
                                }

                                if res.ok {
                                    conn.write().await.handshaked = true;
                                    break true;
                                } else {
                                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                                    break false;
                                }
                            } else {
                                let res = WsResponse::err(
                                    req.id,
                                    "INVALID_REQUEST",
                                    "First message must be connect",
                                );
                                let res_text = serde_json::to_string(&res).unwrap_or_default();
                                if cmd_tx
                                    .send(WsCommand::SendResponse(res_text))
                                    .await
                                    .is_err()
                                {
                                    warn!("[{}] Failed to send invalid-request response", conn_id);
                                }
                                break false;
                            }
                        }
                        Err(e) => {
                            let conn_id = conn.read().await.conn_id.clone();
                            warn!("[{}] Failed to parse frame: {}", conn_id, e);
                            break false;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break false,
                Some(Err(_)) => break false,
                _ => {}
            }
        };

        if !handshake_ok {
            let conn_id = conn.read().await.conn_id.clone();
            info!("[{}] Handshake failed, disconnecting", conn_id);
            return;
        }

        loop {
            let msg = ws_receiver.next().await;

            match msg {
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Ping(data))) => {
                    let conn_id = conn.read().await.conn_id.clone();
                    debug!("[{}] Received ping: {:?}", conn_id, data);
                }
                Some(Ok(Message::Text(text))) => {
                    let conn_id = conn.read().await.conn_id.clone();
                    debug!("[{}] Received: {}", conn_id, text);

                    match serde_json::from_str::<WsRequest>(&text) {
                        Ok(req) => {
                            let res = dispatch_method(&req, &conn, &state, &cmd_tx).await;
                            let res_text = serde_json::to_string(&res).unwrap_or_default();
                            if cmd_tx
                                .send(WsCommand::SendResponse(res_text))
                                .await
                                .is_err()
                            {
                                warn!("[{}] Failed to send response, connection closed", conn_id);
                                break;
                            }
                        }
                        Err(e) => {
                            let conn_id = conn.read().await.conn_id.clone();
                            warn!("[{}] Failed to parse request: {}", conn_id, e);
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
                None => break,
            }
        }

        let conn_id = conn.read().await.conn_id.clone();
        info!("[{}] WebSocket disconnected", conn_id);
    });
    task_registry
        .insert_join(format!("{}:recv", conn_task_prefix), recv_task)
        .await;

    let send_task_name = format!("{}:send", conn_task_prefix);
    let recv_task_name = format!("{}:recv", conn_task_prefix);

    let send_join = match task_registry.remove_join_or_abort(&send_task_name).await {
        Some(h) => h,
        None => {
            warn!("[{}] send task missing from registry", conn_id);
            return;
        }
    };
    let recv_join = match task_registry.remove_join_or_abort(&recv_task_name).await {
        Some(h) => h,
        None => {
            warn!("[{}] recv task missing from registry", conn_id);
            return;
        }
    };

    tokio::select! {
        _ = send_join => {}
        _ = recv_join => {}
    }

    task_registry.abort_matching(&conn_task_prefix).await;

    info!("[{}] WebSocket session ended", conn_id);
}

async fn dispatch_method(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    let scopes = conn.read().await.scopes.clone();
    if let Some(required) = method_scope(&req.method) {
        if !scopes_allow(&scopes, &req.method) {
            if req.method == "commands.execute" {
                if let Some(ref params_val) = req.params {
                    if let Ok(params) =
                        serde_json::from_value::<serde_json::Value>(params_val.clone())
                    {
                        if let Some(session_id) = params.get("session_id").and_then(|v| v.as_str())
                        {
                            let command = params
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let user_text = format!("/{}", command);
                            let error_text =
                                format!("Command error: Missing required scope: {}", required);
                            if let Some(ref store) = state.agents.store {
                                if let Err(e) = store
                                    .append_message(&AppendMessageParams {
                                        session_id,
                                        role: "user",
                                        content: &user_text,
                                        ..Default::default()
                                    })
                                    .await
                                {
                                    warn!("Failed to append user command message: {}", e);
                                }
                                if let Err(e) = store
                                    .append_message(&AppendMessageParams {
                                        session_id,
                                        role: "assistant",
                                        content: &error_text,
                                        ..Default::default()
                                    })
                                    .await
                                {
                                    warn!("Failed to append assistant error message: {}", e);
                                }
                            }
                        }
                    }
                }
            }
            return error_forbidden(&req.id, required);
        }
    }

    match req.method.as_str() {
        "ping" => handshake::handle_ping(req),
        "connect" => {
            WsResponse::err(&req.id, "INVALID_REQUEST", "connect can only be sent as first message")
        }
        "chat.send" => chat::handle_chat_send(req, conn, state).await,
        "chat.history" => chat::handle_chat_history(req, conn, state).await,
        "chat.abort" => chat::handle_chat_abort(req, conn, state).await,
        "feedback.vote" => feedback::handle_feedback_vote(req, state).await,
        "feedback.ops" => feedback::handle_feedback_ops(req, state).await,
        "ask.respond" => ask::handle_ask_respond(req, state).await,
        "sessions.list" => sessions::handle_sessions_list(req, state).await,
        "sessions.create" => sessions::handle_sessions_create(req, conn, state).await,
        "sessions.delete" => sessions::handle_sessions_delete(req, conn, state).await,
        "sessions.rename" => sessions::handle_sessions_rename(req, conn, state).await,
        "sessions.set_pinned" => sessions::handle_sessions_set_pinned(req, conn, state).await,
        "sessions.set_model" => sessions::handle_sessions_set_model(req, conn, state).await,
        "sessions.reset" => sessions::handle_sessions_reset(req, conn, state).await,
        "sessions.subscribe" => sessions::handle_sessions_subscribe(req, conn, cmd_tx).await,
        "sessions.unsubscribe" => sessions::handle_sessions_unsubscribe(req, conn, cmd_tx).await,
        "agents.list" => agents::handle_agents_list(req, state).await,
        "agents.create" => admin_ws::handle_agents_create(req, state).await,
        "agents.delete" => admin_ws::handle_agents_delete(req, state).await,
        "agents.get" => agents::handle_agents_get(req, state).await,
        "agents.registry" => agents::handle_agents_registry(req, state).await,
        "agents.get_config" => agents::handle_agents_get_config(req, state).await,
        "agents.update" => agents::handle_agents_update(req, state).await,
        "agents.default" => agents::handle_agents_default(req, state).await,
        "agents.memory.get" => agents::handle_agents_memory_get(req, state).await,
        "agents.memory.clear" => agents::handle_agents_memory_clear(req, state).await,
        "agents.export" => agents::handle_agents_export(req, state).await,
        "agents.import" => agents::handle_agents_import(req, state).await,
        "health" => agents::handle_health(req, state).await,
        "system.presence" => agents::handle_system_presence(req).await,
        "commands.list" => {
            WsResponse::ok(&req.id, crate::gateway::commands::handle_commands_list())
        }
        "commands.execute" => {
            crate::gateway::commands::handle_commands_execute(req, conn, state).await
        }
        "config.get" => config_ws::handle_config_get(req, state).await,
        "config.set" => config_ws::handle_config_set(req, state).await,
        "eval.optimizer.run" => eval_ws::handle_eval_optimizer_run(req, state).await,
        "eval.optimizer.status" => eval_ws::handle_eval_optimizer_status(req, state).await,
        "eval.optimizer.resume" => eval_ws::handle_eval_optimizer_resume(req, state).await,
        "eval.optimizer.rollback" => eval_ws::handle_eval_optimizer_rollback(req, state).await,
        "eval.trace.list" => eval_ws::handle_eval_trace_list(req, state).await,
        "eval.dashboard" => eval_ws::handle_eval_dashboard(req, state).await,
        "eval.propose" => eval_ws::handle_eval_propose(req, state).await,
        "models.list" => models::handle_models_list(req, state).await,
        "models.presets" => models::handle_models_presets(req, state).await,
        "models.fetch_remote" => models::handle_models_fetch_remote(req, state).await,
        "models.add" => models::handle_models_add(req, state).await,
        "models.remove" => models::handle_models_remove(req, state).await,
        "models.set_default" => models::handle_models_set_default(req, state).await,
        "mcp.list" => mcp_ws::handle_mcp_list(req, state).await,
        "mcp.tools" => admin_ws::handle_mcp_tools(req, state).await,
        "mcp.call_tool" => admin_ws::handle_mcp_call_tool(req, state).await,
        "mcp.resources" => admin_ws::handle_mcp_resources(req, state).await,
        "mcp.auth_status" => admin_ws::handle_mcp_auth_status(req, state).await,
        "mcp.presets" => mcp_ws::handle_mcp_presets(req, state).await,
        "mcp.add" => mcp_ws::handle_mcp_add(req, state).await,
        "mcp.remove" => mcp_ws::handle_mcp_remove(req, state).await,
        "mcp.connect" => mcp_ws::handle_mcp_connect(req, state).await,
        "mcp.disconnect" => mcp_ws::handle_mcp_disconnect(req, state).await,
        "mcp.auth_cancel" => mcp_ws::handle_mcp_auth_cancel(req, state).await,
        "device.capabilities" => device_ws::handle_device_capabilities(req, state).await,
        "device.permission.status" => device_ws::handle_device_permission_status(req, state).await,
        "device.pairing.pending" => admin_ws::handle_device_pairing_pending(req, state).await,
        "device.pairing.authorized" => admin_ws::handle_device_pairing_authorized(req, state).await,
        "device.pairing.approve" => admin_ws::handle_device_pairing_approve(req, state).await,
        "device.pairing.reject" => admin_ws::handle_device_pairing_reject(req, state).await,
        "device.pairing.revoke" => admin_ws::handle_device_pairing_revoke(req, state).await,
        "device.pairing.qr" => admin_ws::handle_device_pairing_qr(req, state).await,
        "device.pairing.setup" => admin_ws::handle_device_pairing_setup(req, state).await,
        "device.permission.request" => {
            device_ws::handle_device_permission_request(req, state).await
        }
        "device.adb.status" => device_ws::handle_device_adb_status(req, state).await,
        "device.adb.pair" => device_ws::handle_device_adb_pair(req, state).await,
        "device.shortcut.run" => device_ws::handle_device_shortcut_run(req, state).await,
        "device.shortcut.results" => device_ws::handle_device_shortcut_results(req, state).await,
        "device.shortcut.inbox" => device_ws::handle_device_shortcut_inbox(req, state).await,
        "cron.list" => tasks::handle_cron_list(req, state).await,
        "tasks.schedule" => tasks::handle_tasks_schedule(req, state).await,
        "tasks.list" => tasks::handle_tasks_list(req, state).await,
        "tasks.delete" => tasks::handle_tasks_delete(req, state).await,
        "tasks.enable" => tasks::handle_tasks_enable(req, state).await,
        "tasks.disable" => tasks::handle_tasks_disable(req, state).await,
        "skills.list" => skills_ws::handle_skills_list(req, state).await,
        "skills.install" => skills_ws::handle_skills_install(req, state).await,
        "connectors.list" => connectors_ws::handle_connectors_list(req, state).await,
        "connectors.install" => connectors_ws::handle_connectors_install(req, state).await,
        "connectors.enable" => connectors_ws::handle_connectors_enable(req, state).await,
        "connectors.disable" => connectors_ws::handle_connectors_disable(req, state).await,
        "connectors.uninstall" => connectors_ws::handle_connectors_uninstall(req, state).await,
        "connectors.auth_status" => connectors_ws::handle_connectors_auth_status(req, state).await,
        "connectors.updates" => connectors_ws::handle_connectors_updates(req, state).await,
        "logs.subscribe" => logs::handle_logs_subscribe(req, conn, state, cmd_tx).await,
        "logs.unsubscribe" => logs::handle_logs_unsubscribe(req, conn, state).await,
        "workspace.list" => workspace::handle_workspace_list(req, state).await,
        "workspace.read" => workspace::handle_workspace_read(req, state).await,
        "acp.list" => acp::handle_acp_list(req, state).await,
        "acp.spawn" => acp::handle_acp_spawn(req, conn, state).await,
        "acp.terminate" => acp::handle_acp_terminate(req, state).await,
        "acp.message" => acp::handle_acp_message(req, state).await,
        "acp.status" => acp::handle_acp_status(req, state).await,
        "acp.pause" => acp::handle_acp_pause(req, state).await,
        "acp.resume" => acp::handle_acp_resume(req, state).await,
        "acp.step" => acp::handle_acp_step(req, state).await,
        "acp.cancel" => acp::handle_acp_cancel(req, state).await,
        "acp.tree" => acp::handle_acp_tree(req, state).await,
        "acp.execute.session" => acp::handle_acp_execute_session(req, state).await,
        "acp.execute.run" => acp::handle_acp_execute_run(req, state).await,
        "permissions.request_macos_accessibility" => {
            handle_permissions_request_macos_accessibility(req).await
        }
        "subscribe" => sessions::handle_legacy_subscribe(req, conn, cmd_tx).await,
        "unsubscribe" => sessions::handle_legacy_unsubscribe(req, conn, cmd_tx).await,
        "subscribe_all" => {
            conn.write().await.subscriptions.clear();
            WsResponse::ok(&req.id, serde_json::json!({"status": "subscribed_all"}))
        }
        // Admin WS methods (plugins / providers / update / cloud / onboarding /
        // catalog) — see ws/admin_ws.rs.
        "onboarding.status" => admin_ws::handle_onboarding_status(req, state).await,
        "onboarding.apply" => admin_ws::handle_onboarding_apply(req, state).await,
        "connectors.catalog" => admin_ws::handle_connectors_catalog(req, state).await,
        "connectors.catalog_install" => {
            admin_ws::handle_connectors_catalog_install(req, state).await
        }
        "cloud.status" => admin_ws::handle_cloud_status(req, state).await,
        "cloud.subscription" => admin_ws::handle_cloud_subscription(req, state).await,
        "cloud.usage" => admin_ws::handle_cloud_usage(req, state).await,
        "cloud.token" => admin_ws::handle_cloud_token(req, state).await,
        "cloud.logout" => admin_ws::handle_cloud_logout(req, state).await,
        "cloud.kb.list" => admin_ws::handle_cloud_kb_list(req, state).await,
        "cloud.kb.create" => admin_ws::handle_cloud_kb_create(req, state).await,
        "cloud.kb.delete" => admin_ws::handle_cloud_kb_delete(req, state).await,
        "cloud.kb.upload" => admin_ws::handle_cloud_kb_upload(req, state).await,
        "cloud.kb.query" => admin_ws::handle_cloud_kb_query(req, state).await,
        "cloud.kb.docs" => admin_ws::handle_cloud_kb_docs(req, state).await,
        "cloud.kb.push" => admin_ws::handle_cloud_kb_push(req, state).await,
        "cloud.kb.pull" => admin_ws::handle_cloud_kb_pull(req, state).await,
        "cloud.credits.claims" => admin_ws::handle_cloud_credits_claims(req, state).await,
        "cloud.credits.daily_claim" => admin_ws::handle_cloud_credits_daily_claim(req, state).await,
        "cloud.credits.signup_claim" => {
            admin_ws::handle_cloud_credits_signup_claim(req, state).await
        }
        "cloud.credits.packs" => admin_ws::handle_cloud_credits_packs(req, state).await,
        "cloud.credits.ledger" => admin_ws::handle_cloud_credits_ledger(req, state).await,
        "cloud.credits.invite" => admin_ws::handle_cloud_credits_invite(req, state).await,
        "cloud.credits.invite_redeem" => {
            admin_ws::handle_cloud_credits_invite_redeem(req, state).await
        }
        "kb.collections" => kb_ws::handle_kb_collections(req, state).await,
        "kb.docs" => kb_ws::handle_kb_docs(req, state).await,
        "kb.doc_content" => kb_ws::handle_kb_doc_content(req, state).await,
        "kb.ingest" => kb_ws::handle_kb_ingest(req, state).await,
        "kb.delete_doc" => kb_ws::handle_kb_delete_doc(req, state).await,
        "update.status" => admin_ws::handle_update_status(req, state).await,
        "update.progress" => admin_ws::handle_update_progress(req, state).await,
        "update.trigger" => admin_ws::handle_update_trigger(req, state).await,
        "plugins.list" => admin_ws::handle_plugins_list(req, state).await,
        "plugins.enable" => admin_ws::handle_plugins_set_enabled(req, state, true).await,
        "plugins.disable" => admin_ws::handle_plugins_set_enabled(req, state, false).await,
        "plugins.install" => admin_ws::handle_plugins_install(req, state).await,
        "plugins.sign" => admin_ws::handle_plugins_sign(req, state).await,
        "plugins.search" => admin_ws::handle_plugins_search(req, state).await,
        "plugins.unload" => admin_ws::handle_plugins_unload(req, state).await,
        "plugins.reload" => admin_ws::handle_plugins_reload(req, state).await,
        "plugins.reload_all" => admin_ws::handle_plugins_reload_all(req, state).await,
        "system.reload" => admin_ws::handle_system_reload(req, state).await,
        "channels.list" => admin_ws::handle_channels_list(req, state).await,
        "channels.enable" => admin_ws::handle_channels_enable(req, state).await,
        "channels.disable" => admin_ws::handle_channels_disable(req, state).await,
        "plugins.uninstall" => admin_ws::handle_plugins_uninstall(req, state).await,
        "providers.list" => admin_ws::handle_providers_list(req, state).await,
        "providers.enable" => admin_ws::handle_providers_set_enabled(req, state, true).await,
        "providers.disable" => admin_ws::handle_providers_set_enabled(req, state, false).await,
        "providers.usage" => admin_ws::handle_providers_usage(req, state).await,
        "providers.health" => admin_ws::handle_providers_health(req, state).await,
        "providers.fallback" => admin_ws::handle_providers_fallback(req, state).await,
        "security.gate.set" => admin_ws::handle_security_gate_set(req, state).await,
        "security.gate.list" => admin_ws::handle_security_gate_list(req, state).await,
        "security.gate.clear" => admin_ws::handle_security_gate_clear(req, state).await,
        "security.allowlist.add" => admin_ws::handle_security_allowlist_add(req, state).await,
        "security.allowlist.remove" => admin_ws::handle_security_allowlist_remove(req, state).await,
        "security.allowlist.list" => admin_ws::handle_security_allowlist_list(req, state).await,
        "security.status" => admin_ws::handle_security_status(req, state).await,
        "providers.check" => admin_ws::handle_providers_check(req, state).await,
        "providers.switch" => admin_ws::handle_providers_switch(req, state).await,
        "models.default" => admin_ws::handle_models_default(req, state).await,
        "traces.get" => admin_ws::handle_traces_get(req, state).await,
        "status.get" => admin_ws::handle_status_get(req, state).await,
        "audit.recent" => admin_ws::handle_audit_recent(req, state).await,
        "audit.all" => admin_ws::handle_audit_all(req, state).await,
        "approvals.list" => admin_ws::handle_approvals_list(req, state).await,
        "approvals.get" => admin_ws::handle_approvals_get(req, state).await,
        "approvals.approve" => admin_ws::handle_approvals_approve(req, state).await,
        "approvals.deny" => admin_ws::handle_approvals_deny(req, state).await,
        "memory.search" => admin_ws::handle_memory_search(req, state).await,
        "memory.add" => admin_ws::handle_memory_add(req, state).await,
        "memory.collections" => admin_ws::handle_memory_collections(req, state).await,
        "mention.policy" => admin_ws::handle_mention_policy_get(req, state).await,
        "mention.policy.set" => admin_ws::handle_mention_policy_set(req, state).await,
        "mention.allowlist" => admin_ws::handle_mention_allowlist_list(req, state).await,
        "mention.allowlist.add" => admin_ws::handle_mention_allowlist_add(req, state).await,
        "mention.allowlist.remove" => admin_ws::handle_mention_allowlist_remove(req, state).await,
        "mention.blocklist" => admin_ws::handle_mention_blocklist_list(req, state).await,
        "mention.blocklist.add" => admin_ws::handle_mention_blocklist_add(req, state).await,
        "mention.blocklist.remove" => admin_ws::handle_mention_blocklist_remove(req, state).await,
        "auth_profiles.list" => admin_ws::handle_auth_profiles_list(req, state).await,
        "auth_profiles.get" => admin_ws::handle_auth_profiles_get(req, state).await,
        "auth_profiles.rotate" => admin_ws::handle_auth_profiles_rotate(req, state).await,
        "cron.get" => admin_ws::handle_cron_get(req, state).await,
        "cron.enable" => admin_ws::handle_cron_set_enabled(req, state, true).await,
        "cron.disable" => admin_ws::handle_cron_set_enabled(req, state, false).await,
        "cron.run" => admin_ws::handle_cron_run(req, state).await,
        "cron.add" => admin_ws::handle_cron_add(req, state).await,
        "cron.remove" => admin_ws::handle_cron_remove(req, state).await,
        "cron.logs" => admin_ws::handle_cron_logs(req, state).await,
        "skills.get" => admin_ws::handle_skills_get(req, state).await,
        "skills.enable" => admin_ws::handle_skills_set_enabled(req, state, true).await,
        "skills.disable" => admin_ws::handle_skills_set_enabled(req, state, false).await,
        "skills.uninstall" => admin_ws::handle_skills_uninstall(req, state).await,
        "skills.run" => admin_ws::handle_skills_run(req, state).await,
        _ => error_method_not_found(&req.id, &req.method),
    }
}

async fn handle_permissions_request_macos_accessibility(req: &WsRequest) -> WsResponse {
    #[cfg(target_os = "macos")]
    {
        crate::computer::platform::macos::permissions::trigger_accessibility_prompt();
        crate::computer::platform::macos::permissions::open_accessibility_settings();
        WsResponse::ok(
            &req.id,
            serde_json::json!({
                "status": "prompt_triggered",
                "message": "System permission dialog triggered. Please allow access in System Settings → Privacy & Security → Accessibility, then restart Syscity."
            }),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        WsResponse::err(
            &req.id,
            "UNSUPPORTED_PLATFORM",
            "This permission request is only available on macOS",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::protocol::AuthMode;
    use crate::gateway::state_tests::{make_test_conn, make_test_state};
    use crate::gateway::GatewayConfig;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    fn req(id: &str, method: &str, params: Option<serde_json::Value>) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    async fn dispatch(
        conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
        r: &WsRequest,
    ) -> WsResponse {
        let state = state().await;
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<WsCommand>(1);
        dispatch_method(r, conn, &state, &cmd_tx).await
    }

    // ── dispatch_method ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_unknown_method_with_admin_scope_not_found() {
        let conn = make_test_conn(&["admin"]);
        let resp = dispatch(&conn, &req("r1", "bogus.method", None)).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "METHOD_NOT_FOUND");
    }

    #[tokio::test]
    async fn dispatch_unknown_method_without_scope_forbidden() {
        // Unknown methods default-deny to admin scope.
        let conn = make_test_conn(&[]);
        let resp = dispatch(&conn, &req("r1", "bogus.method", None)).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "FORBIDDEN");
    }

    #[tokio::test]
    async fn dispatch_read_method_without_scope_forbidden() {
        let conn = make_test_conn(&[]);
        let resp = dispatch(&conn, &req("r1", "health", None)).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "FORBIDDEN");
    }

    #[tokio::test]
    async fn dispatch_commands_execute_without_scope_forbidden() {
        // Exercises the commands.execute scope-denied special case that tries
        // to append a user + assistant error message pair (no store here, so
        // the append is skipped).
        let conn = make_test_conn(&[]);
        let params = Some(serde_json::json!({ "session_id": "s1", "command": "status" }));
        let resp = dispatch(&conn, &req("r1", "commands.execute", params)).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "FORBIDDEN");
    }

    #[tokio::test]
    async fn dispatch_ping_ok_without_scope() {
        let conn = make_test_conn(&[]);
        let resp = dispatch(&conn, &req("r1", "ping", None)).await;
        assert!(resp.ok);
        assert!(resp.payload.as_ref().unwrap().is_object());
    }

    #[tokio::test]
    async fn dispatch_connect_after_handshake_errors() {
        let conn = make_test_conn(&[]);
        let resp = dispatch(&conn, &req("r1", "connect", None)).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn dispatch_agents_list_with_admin_scope_routes() {
        let conn = make_test_conn(&["admin"]);
        let resp = dispatch(&conn, &req("r1", "agents.list", None)).await;
        assert!(resp.ok);
        assert!(resp.payload.as_ref().unwrap()["agents"].is_array());
    }

    #[tokio::test]
    async fn dispatch_health_with_read_scope_ok() {
        let conn = make_test_conn(&["read"]);
        let resp = dispatch(&conn, &req("r1", "health", None)).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "healthy");
    }

    #[tokio::test]
    async fn dispatch_subscribe_all_ok() {
        let conn = make_test_conn(&["admin"]);
        let resp = dispatch(&conn, &req("r1", "subscribe_all", None)).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "subscribed_all");
    }

    // ── ws_auth_middleware ───────────────────────────────────────────────────

    async fn middleware_app(state: Arc<GatewayState>) -> axum::Router {
        axum::Router::new()
            .route("/ws", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), ws_auth_middleware))
    }

    #[tokio::test]
    async fn auth_middleware_anonymous_mode_allows() {
        let state = state().await;
        let app = middleware_app(state).await;
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ws")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    async fn token_state() -> Arc<GatewayState> {
        let mut config = GatewayConfig::default();
        config.security.auth_mode = AuthMode::Token;
        config.security.shared_token = Some("secret-token".to_string());
        Arc::new(make_test_state(config).await)
    }

    #[tokio::test]
    async fn auth_middleware_token_mode_rejects_without_credentials() {
        let state = token_state().await;
        let app = middleware_app(state).await;
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ws")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_middleware_bearer_token_allows() {
        let state = token_state().await;
        let app = middleware_app(state).await;
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ws")
                    .header(axum::http::header::AUTHORIZATION, "Bearer secret-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_middleware_query_token_allows() {
        let state = token_state().await;
        let app = middleware_app(state).await;
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ws?token=secret-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── platform-gated permission handler ────────────────────────────────────

    #[tokio::test]
    async fn permissions_macos_accessibility_platform_gated() {
        let resp = handle_permissions_request_macos_accessibility(&req("r1", "x", None)).await;
        if cfg!(target_os = "macos") {
            assert!(resp.ok);
            assert_eq!(resp.payload.as_ref().unwrap()["status"], "prompt_triggered");
        } else {
            assert!(!resp.ok);
            assert_eq!(resp.error.as_ref().unwrap().code, "UNSUPPORTED_PLATFORM");
        }
    }
}
