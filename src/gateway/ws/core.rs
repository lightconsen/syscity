//! WebSocket connection lifecycle: auth middleware, upgrade validation,
//! the per-connection event loop, and the method dispatcher.

use super::*;

/// Seconds a connection has to complete its `connect` handshake before the
/// gateway hangs up. Real clients handshake in milliseconds; anything still
/// quiet after this is wedged or a probe.
const HANDSHAKE_TIMEOUT_SECS: u64 = 30;

/// Middleware: validate WebSocket upgrade credentials before proceeding.
///
/// Runs BEFORE the WebSocket upgrade. When auth_mode is not "none", rejects
/// with 401 if no valid Bearer session token, shared token, or query token is
/// found. There is no cookie credential: the gateway never issues cookies.
pub async fn ws_auth_middleware(
    State(state): State<Arc<GatewayState>>,
    mut req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    // Before credentials: is this upgrade coming from somewhere we will talk
    // to at all? A browser reaches a loopback port from any page it likes, so
    // the Origin and Host headers are the only thing standing between the local
    // runtime and a drive-by connection.
    let (auth_mode, bound_host, port, allowed_origins) = {
        let config = state.config.read().await;
        (
            config.security.auth_mode,
            config.host.clone(),
            config.port,
            config.security.allowed_ws_origins.clone(),
        )
    };
    if let Err(reason) = crate::gateway::auth::ws_origin::check_upgrade(
        req.headers(),
        &bound_host,
        port,
        &allowed_origins,
    ) {
        warn!("WebSocket upgrade rejected: {}", reason);
        return axum::http::Response::builder()
            .status(axum::http::StatusCode::FORBIDDEN)
            .body(axum::body::Body::from(format!("Forbidden: {reason}")))
            .unwrap_or_else(|_| {
                axum::http::Response::new(axum::body::Body::from("Forbidden".to_string()))
            });
    }

    if matches!(auth_mode, crate::gateway::protocol::AuthMode::None) {
        // Anonymous local access. The entitlement is what the *config* says a
        // local client is worth (`security.local_scopes`) — not a fixed pair
        // and certainly not anything the client sends; the handshake narrows
        // this further if the client asks for less.
        let local_scopes = {
            let config = state.config.read().await;
            config.security.local_scopes.clone()
        };
        req.extensions_mut().insert(WsAuthResult {
            user_id: UserId::new("anonymous"),
            scopes: local_scopes,
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
                    scopes: config.security.shared_token_scopes.clone(),
                });
            }
        }
        // Check query parameter token
        if let Some(qt) = query_token {
            if qt == shared_token {
                return Ok(WsAuthResult {
                    user_id: UserId::new("shared"),
                    scopes: config.security.shared_token_scopes.clone(),
                });
            }
        }
    }

    warn!("WebSocket upgrade rejected: no valid credentials");
    let resp = axum::http::Response::builder()
        .status(axum::http::StatusCode::UNAUTHORIZED)
        .header(axum::http::header::WWW_AUTHENTICATE, "Bearer")
        .body(axum::body::Body::from("Unauthorized: a valid API token is required"))
        .unwrap_or_else(|_| {
            axum::http::Response::new(axum::body::Body::from(
                "Unauthorized: a valid API token is required",
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

    // Refuse the upgrade outright once the connection cap is reached — a
    // client that has not even completed its handshake must not hold a slot
    // forever while others queue behind it.
    match try_take_connection_slot(&state).await {
        Some(slot) => ws.on_upgrade(move |socket| {
            handle_websocket(socket, state, query, auth_mode, auth_result, slot)
        }),
        None => {
            warn!(
                "WebSocket upgrade refused: connection cap ({}) reached",
                state.config.read().await.security.max_ws_connections,
            );
            const BUSY: &str = "Too many connections";
            axum::http::Response::builder()
                .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                .body(axum::body::Body::from(BUSY))
                .unwrap_or_else(|_| axum::http::Response::new(axum::body::Body::from(BUSY)))
        }
    }
}

/// Reserve a slot in the connection cap, holding it if under the limit.
///
/// `Some(slot)` means the connection is one of the `max_ws_connections`
/// allowed, and `slot` carries the counter back down when the connection ends.
/// `None` means the cap is reached and no slot was taken.
async fn try_take_connection_slot(state: &Arc<GatewayState>) -> Option<ConnectionSlot> {
    let current = state
        .active_ws_connections
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cap = state.config.read().await.security.max_ws_connections;
    if current >= cap {
        state
            .active_ws_connections
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    }
    Some(ConnectionSlot { state: state.clone() })
}

/// A connection's slot in the [`GatewayState`]`::active_ws_connections` cap.
///
/// The counter is incremented by [`try_take_connection_slot`]; this guard
/// lives for the whole of [`handle_websocket`] and releases its slot when the
/// session ends — on every exit path, including a panic.
struct ConnectionSlot {
    state: Arc<GatewayState>,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.state
            .active_ws_connections
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Which connections may receive a gateway event.
///
/// The classification is exhaustive on purpose: adding a `GatewayEvent`
/// variant stops this file compiling until its audience is decided, so a new
/// event cannot quietly inherit "send to everyone".
#[derive(Debug, Clone, PartialEq, Eq)]
enum Audience {
    /// Operator-level fact; every handshaked connection may see it.
    All,
    /// One session's business: only connections subscribed to it.
    Session(String),
    /// Carries a device pairing code, which is the key to a device: only
    /// connections granted the `pairing` scope.
    Pairing,
}

/// Decide who may receive `event`.
fn audience_of(event: &GatewayEvent) -> Audience {
    use GatewayEvent as E;
    let session = |id: &str| Audience::Session(id.to_string());
    match event {
        // ── One session's business ──────────────────────────────────────────
        E::AgentResponse { session_id, .. }
        | E::Thinking { session_id, .. }
        | E::ContentDelta { session_id, .. }
        | E::ToolCalling { session_id, .. }
        | E::ToolResult { session_id, .. }
        | E::Completed { session_id, .. }
        | E::ProcessingError { session_id, .. }
        | E::GoalProgress { session_id, .. }
        | E::SessionCreated { session_id, .. }
        | E::SessionRenamed { session_id, .. }
        | E::SessionPinned { session_id, .. }
        | E::SessionModelChanged { session_id, .. }
        | E::AcpSpawned { session_id, .. }
        | E::AcpCompleted { session_id, .. }
        | E::AcpStatusChanged { session_id, .. }
        | E::AcpRecovered { session_id, .. } => session(session_id),
        E::AskRequired(e) => session(&e.session_id),
        E::AskResolved(e) => session(&e.session_id),

        // ── The pairing code ────────────────────────────────────────────────
        // Whoever can act on it, and no one else: the code is what lets a
        // device in.
        E::DevicePairRequested { .. } => Audience::Pairing,

        // ── Operator-level facts the UI renders ─────────────────────────────
        // Deliberate, not accidental: these have no session to key on (the
        // approval event carries no session id) and every client that shows
        // them is the operator's own.
        E::ApprovalRequired { .. }
        | E::AgentStatus { .. }
        | E::ChannelStatus { .. }
        | E::CronAnnounce { .. }
        | E::RepairAction { .. }
        | E::DeviceStatusChanged { .. }
        | E::ConnectorChanged { .. }
        | E::McpConnected { .. }
        | E::McpDisconnected { .. }
        | E::McpRecovered { .. }
        | E::McpResourceChanged { .. }
        | E::McpAuthRequired { .. }
        | E::McpAuthComplete { .. }
        | E::McpAuthFailed { .. }
        | E::McpTokenRefreshed { .. }
        | E::AcpThreadSwitched { .. }
        | E::MessageReceived { .. } => Audience::All,
    }
}

async fn handle_websocket(
    socket: WebSocket,
    state: Arc<GatewayState>,
    query: WsConnectQuery,
    auth_mode: crate::gateway::protocol::AuthMode,
    auth_result: WsAuthResult,
    _connection_slot: ConnectionSlot,
) {
    let conn_id = Uuid::new_v4().to_string();
    info!("[{}] WebSocket connected", conn_id);

    // `_connection_slot` lives for this whole function: one slot per socket,
    // taken at the upgrade in `ws_handler`, released when the session ends.

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
                recv = event_rx.recv() => {
                    // A lagged receiver used to fall through the `Ok(event)`
                    // pattern into `else => break`: the connection closed with
                    // no explanation, indistinguishable from a network blip.
                    // Say what happened first, then close — the client's
                    // reconnect-and-reload is the only sound recovery, but it
                    // should be able to choose it deliberately.
                    let event = match recv {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                            let (conn_id, handshaked) = {
                                let guard = conn_send.read().await;
                                (guard.conn_id.clone(), guard.handshaked)
                            };
                            warn!(
                                "[{}] Event stream lagged, {} event(s) dropped — closing so the \
                                 client can resync",
                                conn_id, missed
                            );
                            // Only a handshaked session has a stream to resync:
                            // before that the client has not been told which one
                            // it is on, and an event frame ahead of its connect
                            // response would only be noise.
                            if handshaked {
                                let seq = conn_send.write().await.next_seq();
                                let notice = WsEvent::new(
                                    "stream.lagged",
                                    serde_json::json!({ "missed": missed }),
                                    seq,
                                );
                                match serde_json::to_string(&notice) {
                                    Ok(text) => {
                                        let _ = ws_sender.send(Message::Text(text)).await;
                                    }
                                    Err(e) => warn!(
                                        "[{}] Failed to serialize stream.lagged notice: {}",
                                        conn_id, e
                                    ),
                                }
                            }
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };

                    let conn_guard = conn_send.read().await;
                    if !conn_guard.handshaked {
                        continue;
                    }

                    let should_send = match audience_of(&event) {
                        Audience::All => true,
                        Audience::Session(session_id) => conn_guard.is_subscribed(&session_id),
                        Audience::Pairing => conn_guard.scopes.iter().any(|s| {
                            s == crate::gateway::protocol::SCOPE_PAIRING
                                || s == crate::gateway::protocol::SCOPE_ADMIN
                        }),
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
                        match serde_json::to_string(&ws_event) {
                            Ok(text) => {
                                if ws_sender.send(Message::Text(text)).await.is_err() {
                                    break;
                                }
                            }
                            // The event's payload did not serialize; the
                            // connection is fine, so it stays open and the
                            // drop is at least on the record.
                            Err(e) => {
                                warn!("Failed to serialize '{}' event: {}", ws_event.event, e)
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
        // A connection that never completes its handshake must not hold a
        // socket — or a connection-cap slot — forever. A real client sends
        // `connect` within milliseconds of the upgrade; anything still quiet
        // after this is either wedged or a probe.
        let handshake_ok = match tokio::time::timeout(
            std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
            async {
                loop {
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
                                        let res_text = response_frame(&res);
                                        if cmd_tx
                                            .send(WsCommand::SendResponse(res_text))
                                            .await
                                            .is_err()
                                        {
                                            warn!(
                                                "[{}] Failed to send handshake response",
                                                conn_id
                                            );
                                            break false;
                                        }

                                        if res.ok {
                                            conn.write().await.handshaked = true;
                                            break true;
                                        } else {
                                            tokio::time::sleep(tokio::time::Duration::from_secs(1))
                                                .await;
                                            break false;
                                        }
                                    } else {
                                        let res = WsResponse::err(
                                            req.id,
                                            "INVALID_REQUEST",
                                            "First message must be connect",
                                        );
                                        let res_text = response_frame(&res);
                                        if cmd_tx
                                            .send(WsCommand::SendResponse(res_text))
                                            .await
                                            .is_err()
                                        {
                                            warn!(
                                                "[{}] Failed to send invalid-request response",
                                                conn_id
                                            );
                                        }
                                        break false;
                                    }
                                }
                                Err(e) => {
                                    // Pre-handshake the connection has no
                                    // identity and its first frame is required
                                    // to be `connect`, so a malformed one ends
                                    // it. (Post-handshake the frame is answered
                                    // instead — there the session is worth
                                    // keeping and the client is waiting on a
                                    // request.)
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
                }
            },
        )
        .await
        {
            Ok(handshaken) => handshaken,
            Err(_) => {
                let conn_id = conn.read().await.conn_id.clone();
                warn!(
                    "[{}] Handshake timed out after {}s, disconnecting",
                    conn_id, HANDSHAKE_TIMEOUT_SECS,
                );
                false
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
                            let res =
                                dispatch_method(&req, &conn, &state, &cmd_tx, &auth_mode).await;
                            let res_text = response_frame(&res);
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
                            // Answer it. The client sent an id and is waiting
                            // on its own timeout otherwise; the frame is bad,
                            // not the connection, so keep it open. The id is
                            // salvaged with a lenient parse so the error can be
                            // correlated when it is still readable.
                            let id = serde_json::from_str::<serde_json::Value>(&text)
                                .ok()
                                .and_then(|v| {
                                    v.get("id").and_then(|i| i.as_str()).map(String::from)
                                })
                                .unwrap_or_default();
                            let res = WsResponse::err(
                                id,
                                "INVALID_JSON",
                                format!("Malformed request frame: {}", e),
                            );
                            let res_text = response_frame(&res);
                            if cmd_tx
                                .send(WsCommand::SendResponse(res_text))
                                .await
                                .is_err()
                            {
                                warn!("[{}] Failed to send parse-error response", conn_id);
                                break;
                            }
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

/// Serialize a response frame for the wire, never producing an empty frame.
///
/// `WsResponse` is strings and `serde_json::Value`s, so serializing it cannot
/// actually fail today — but the `unwrap_or_default()` this replaces made the
/// failure mode an *empty* frame, which a client can neither parse nor act on.
/// An error frame is the shape that at least names the problem and keeps the
/// id the caller sent.
fn response_frame(res: &WsResponse) -> String {
    if let Ok(text) = serde_json::to_string(res) {
        return text;
    }
    warn!("Failed to serialize response '{}'", res.id);
    // Built from strings only, so this cannot fail in turn; if it somehow did,
    // an empty frame is no worse than the one we could not build.
    serde_json::to_string(&WsResponse::err(
        res.id.clone(),
        "INTERNAL_ERROR",
        "Response could not be serialized",
    ))
    .unwrap_or_default()
}

async fn dispatch_method(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    cmd_tx: &mpsc::Sender<WsCommand>,
    auth_mode: &crate::gateway::protocol::AuthMode,
) -> WsResponse {
    let scopes = conn.read().await.scopes.clone();

    // Build the per-request identity context once, from the identity resolved
    // at the handshake plus the transport auth mode, and thread it into the
    // handlers that audit or rate-limit. This is pure plumbing: for the
    // single-user/default case the user id is unchanged (`"anonymous"`,
    // `"shared"`, `"tailscale"`, or the device id).
    let ctx = {
        let cg = conn.read().await;
        let source = match auth_mode {
            crate::gateway::protocol::AuthMode::None => AuthSource::None,
            crate::gateway::protocol::AuthMode::Token => AuthSource::SharedToken,
            crate::gateway::protocol::AuthMode::Device => AuthSource::Device,
            crate::gateway::protocol::AuthMode::Tailscale => AuthSource::Tailscale,
        };
        let ctx = RequestContext::from_identity(cg.user_id.as_ref(), source);
        // Under device auth the paired device id *is* the user id, so record it
        // as such rather than making downstream code re-derive it off `conn`.
        // (There is no session id at dispatch time: it lives in the per-method
        // params, so callers that know it attach it via `with_session_id`.)
        if source == AuthSource::Device {
            let device_id = ctx.user_id().to_string();
            ctx.with_device_id(device_id)
        } else {
            ctx
        }
    };
    if let Some(required) = method_scope(&req.method) {
        if !scopes_allow(&scopes, &req.method) {
            // A refused request must not touch anything. This branch used to
            // append a `/cmd` + "Command error: …" pair to the session history
            // for `commands.execute` — a write performed by a request that was
            // rejected for lack of write scope, into a session id taken from
            // the rejected request (which the store would happily create). The
            // caller already receives the error and reports it itself.
            warn!("Rejected {} for user {}: missing scope {}", req.method, ctx.user_id(), required);
            return error_forbidden(&req.id, required);
        }
    }

    match req.method.as_str() {
        "ping" => handshake::handle_ping(req),
        "connect" => {
            WsResponse::err(&req.id, "INVALID_REQUEST", "connect can only be sent as first message")
        }
        "chat.send" => chat::handle_chat_send(req, conn, state, &ctx).await,
        "chat.history" => chat::handle_chat_history(req, conn, state).await,
        "chat.abort" => chat::handle_chat_abort(req, conn, state).await,
        "feedback.vote" => feedback::handle_feedback_vote(req, state).await,
        "feedback.ops" => feedback::handle_feedback_ops(req, state).await,
        "ask.respond" => ask::handle_ask_respond(req, state).await,
        "sessions.list" => sessions::handle_sessions_list(req, state).await,
        "sessions.create" => sessions::handle_sessions_create(req, conn, state, &ctx).await,
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
        "agents.purge" => admin_ws::handle_agents_purge(req, state).await,
        "agents.rename" => admin_ws::handle_agents_rename(req, state).await,
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
            crate::gateway::commands::handle_commands_execute(req, conn, state, &ctx).await
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
        "acp.spawn" => acp::handle_acp_spawn(req, state, &ctx).await,
        "acp.terminate" => acp::handle_acp_terminate(req, state, &ctx).await,
        "acp.message" => acp::handle_acp_message(req, state, &ctx).await,
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
        "cost.get" => admin_ws::handle_cost_get(req, state).await,
        "cost.reset" => admin_ws::handle_cost_reset(req, state).await,
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

    /// A frame sent to a client is never empty: an empty frame cannot be
    /// parsed and is indistinguishable from a protocol bug, which is what the
    /// `unwrap_or_default()` this replaced would have produced.
    #[test]
    fn response_frames_are_never_empty() {
        let frame = response_frame(&WsResponse::ok("r1", serde_json::json!({"n": 1})));
        assert!(!frame.is_empty());
        let value: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(value["id"], "r1");
        assert_eq!(value["ok"], true);
        assert_eq!(value["payload"]["n"], 1);

        let err_frame = response_frame(&WsResponse::err("r2", "E", "boom"));
        let value: serde_json::Value = serde_json::from_str(&err_frame).unwrap();
        assert_eq!(value["id"], "r2");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "E");
    }

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
        dispatch_method(r, conn, &state, &cmd_tx, &crate::gateway::protocol::AuthMode::None).await
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
        // A scope-refused `commands.execute` is answered and nothing else
        // happens: the frame carries the id and the session id it named goes
        // untouched (this used to append a `/cmd` + error pair to the session
        // history it was refused for).
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

    /// One slot per connection is exactly what `max_ws_connections` promises —
    /// a zero cap takes nothing.
    #[tokio::test]
    async fn connection_cap_of_zero_takes_no_slot() {
        let mut config = GatewayConfig::default();
        config.security.max_ws_connections = 0;
        let state = Arc::new(make_test_state(config).await);
        assert!(
            try_take_connection_slot(&state).await.is_none(),
            "a zero cap must refuse the slot"
        );
        assert_eq!(
            state
                .active_ws_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a refused upgrade must not hold a slot"
        );
    }

    /// Up to the cap, every connection gets a slot; past it, none does — and
    /// the counter tracks the currently-held slots exactly.
    #[tokio::test]
    async fn connection_cap_admits_up_to_the_limit() {
        let mut config = GatewayConfig::default();
        config.security.max_ws_connections = 2;
        let state = Arc::new(make_test_state(config).await);

        let slot1 = try_take_connection_slot(&state).await.expect("first fits");
        let slot2 = try_take_connection_slot(&state).await.expect("second fits");
        assert!(try_take_connection_slot(&state).await.is_none(), "third is refused");
        assert_eq!(
            state
                .active_ws_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );

        drop(slot1);
        assert_eq!(
            state
                .active_ws_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "closing a connection frees its slot"
        );
        drop(slot2);
        assert_eq!(
            state
                .active_ws_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    /// The guard releases its slot on drop — the atomic comes back to zero
    /// even when the connection path ends in a panic (represented here by
    /// dropping the guard early).
    #[tokio::test]
    async fn connection_slot_releases_on_drop() {
        let config = GatewayConfig::default();
        let state = Arc::new(make_test_state(config).await);
        state
            .active_ws_connections
            .store(1, std::sync::atomic::Ordering::Relaxed);
        {
            let _slot = ConnectionSlot { state: state.clone() };
        }
        assert_eq!(
            state
                .active_ws_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "dropping the slot must release the counter"
        );
    }

    /// The classification is what decides who sees an event; the exhaustive
    /// match is the compile-time guard (a new variant fails to build until it
    /// is placed), and these are the runtime assertions for each family.
    #[test]
    fn events_are_classified_by_audience() {
        use crate::gateway::runtime::GatewayEvent as E;

        let session_events = vec![
            E::ContentDelta {
                session_id: "s1".into(),
                agent_id: "default".into(),
                delta: "x".into(),
            },
            E::SessionRenamed {
                session_id: "s1".into(),
                name: "n".into(),
            },
            E::SessionPinned {
                session_id: "s1".into(),
                pinned: true,
            },
            E::Thinking {
                session_id: "s1".into(),
                agent_id: "default".into(),
                content: Some("t".into()),
            },
        ];
        for event in session_events {
            assert_eq!(
                audience_of(&event),
                Audience::Session("s1".to_string()),
                "{event:?} is one session's business"
            );
        }

        // A pairing code is the key to a device: scope-gated, never broadcast.
        assert_eq!(
            audience_of(&E::DevicePairRequested {
                device_id: "d1".into(),
                code: "ABCD1234".into(),
                display_name: None,
            }),
            Audience::Pairing
        );

        // Operator-level facts the UI renders stay broadcast.
        assert_eq!(
            audience_of(&E::AgentStatus {
                agent_id: "default".into(),
                status: crate::gateway::AgentStatus::Idle,
            }),
            Audience::All
        );
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
