//! Session list/create/delete/rename/pin/reset + subscribe/unsubscribe (incl. legacy).

use chrono::Utc;

use super::*;
pub(super) async fn handle_sessions_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let sessions: Vec<serde_json::Value> = if let Some(ref store) = state.agents.store {
        match store.find_sessions(None, None, None, false).await {
            Ok(rows) => rows
                .into_iter()
                .map(|meta| {
                    let name = meta.name.unwrap_or_else(|| {
                        if meta.message_count == 0 {
                            "New Session".to_string()
                        } else {
                            meta.last_activity.format("%b %d %H:%M").to_string()
                        }
                    });
                    serde_json::json!({
                        "session_id": meta.session_id,
                        "name": name,
                        "agent_id": meta.agent_id,
                        "channel": meta.channel,
                        "message_count": meta.message_count,
                        "last_activity": meta.last_activity.to_rfc3339(),
                        "is_active": meta.is_active,
                        "pinned": meta.pinned,
                        "model": meta.model,
                        "created_at": meta.created_at.to_rfc3339(),
                    })
                })
                .collect(),
            Err(e) => {
                tracing::warn!("Failed to list sessions from store: {}", e);
                Vec::new()
            }
        }
    } else {
        let mgr = state.agents.manager.read().await;
        mgr.list_sessions()
            .await
            .into_iter()
            .map(|id| serde_json::json!({ "session_id": id, "name": id }))
            .collect()
    };

    WsResponse::ok(&req.id, serde_json::json!({ "sessions": sessions }))
}

pub(super) async fn handle_sessions_create(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    let cg = conn.read().await;
    let channel = cg
        .client
        .as_ref()
        .map(|c| c.id.as_str())
        .unwrap_or("ws")
        .to_string();
    drop(cg);

    // Identity comes from the request context rather than being re-derived off
    // the connection: the context already resolves to the handshake identity
    // (`"anonymous"` when unauthenticated), so the two cannot drift apart.
    let user = ctx.user_id().to_string();

    #[derive(Debug, Deserialize)]
    struct CreateParams {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
    }

    let params: CreateParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let session_id = params
        .session_id
        .unwrap_or_else(|| format!("{}:{}", channel, user));

    {
        let mut mgr = state.agents.manager.write().await;
        mgr.create_session(session_id.clone());
    }

    let mut metadata = crate::agent::session_store::SessionMetadata::new(
        &session_id,
        params.agent_id.as_deref().unwrap_or(""),
        &channel,
        &user,
    );
    metadata.bound_agent_id = params.agent_id.clone();

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.save_session(&session_id, &metadata, "{}").await {
            warn!("Failed to save session {}: {}", session_id, e);
        }
    }

    // Bind the session to the requested agent so future messages route there.
    if let Some(agent_id) = params.agent_id {
        if !agent_id.is_empty() {
            let route = crate::inbound::RouteResult {
                agent_id,
                workspace_id: None,
                persisted_binding: true,
                is_fallback: false,
            };
            state.agents.router.bind_session(&session_id, &route).await;
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": session_id,
            "status": "created",
        }),
    )
}

pub(super) async fn handle_sessions_delete(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct DeleteParams {
        session_id: String,
    }

    let params: DeleteParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    {
        let mut mgr = state.agents.manager.write().await;
        mgr.terminate_session(&params.session_id).await;
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.delete_session(&params.session_id).await {
            warn!("Failed to delete session {}: {}", params.session_id, e);
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({ "status": "deleted", "session_id": params.session_id }),
    )
}

/// `sessions.plan` — the plan the session is executing (`{ session_id? }`).
///
/// The plan belongs to the agent, so this asks it rather than reading anything
/// here; an agent with no plan for the session is an answer ("there is none"),
/// not an error, because that is the ordinary state of a conversation.
pub(super) async fn handle_sessions_plan(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    let agent_id = state
        .agents
        .router
        .resolve_by_session(&session_id)
        .await
        .agent_id;
    let handle = {
        let agents = state.agents.agents.read().await;
        agents.get(&agent_id).cloned()
    };
    let Some(handle) = handle else {
        return WsResponse::err(
            &req.id,
            "NOT_FOUND",
            format!("session '{session_id}' is not attached to a running agent"),
        );
    };

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if handle
        .query_tx
        .send(crate::gateway::runtime::AgentQuery::GetPlan {
            conv_id: session_id.clone(),
            response_tx,
        })
        .await
        .is_err()
    {
        return WsResponse::err(
            &req.id,
            "AGENT_UNAVAILABLE",
            format!("agent '{agent_id}' is no longer running"),
        );
    }
    let plan = match response_rx.await {
        Ok(plan) => plan,
        Err(e) => {
            warn!("Agent {agent_id} did not answer a plan request: {e}");
            return WsResponse::err(
                &req.id,
                "AGENT_UNAVAILABLE",
                format!("agent '{agent_id}' did not answer"),
            );
        }
    };

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": session_id,
            "plan": plan,
        }),
    )
}

/// The session a request is about: the one it names, else the one its
/// connection is watching, else a refusal.
///
/// Naming it is optional on purpose — a client subscribed to a single session
/// asks about *that* session, which is what makes the TUI commands read as
/// `/undo` and `/plan` rather than `/undo <id>`.
pub(super) async fn session_id_for(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    _state: &Arc<GatewayState>,
) -> Result<String, WsResponse> {
    #[derive(Debug, Deserialize)]
    struct SessionIdParam {
        #[serde(default)]
        session_id: Option<String>,
    }

    let params: SessionIdParam = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return Err(res),
    };
    if let Some(id) = params.session_id.filter(|id| !id.is_empty()) {
        return Ok(id);
    }
    match conn.read().await.subscriptions.first().cloned() {
        Some(id) => Ok(id),
        None => Err(WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            "no session_id given and this connection is not subscribed to one",
        )),
    }
}

/// `sessions.snapshot` — start recording what this session's writes replace
/// (`{ session_id?, label? }`).
///
/// From here until a rollback, the three tools that name the file they write
/// keep the pre-image of each file they touch, once. That is the whole feature:
/// the content a write destroys cannot be reconstructed afterwards, so "put it
/// back" has to be arranged before the write.
pub(super) async fn handle_sessions_snapshot(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SnapshotParams {
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        #[serde(default)]
        label: Option<String>,
    }

    let params: SnapshotParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    let label = params
        .label
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty());
    match state.infra.checkpoints.begin(&session_id, label).await {
        Ok(checkpoint) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "id": checkpoint.id,
                "label": checkpoint.label,
                "created_at": checkpoint.created_at.to_rfc3339(),
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "SNAPSHOT_FAILED", e.to_string()),
    }
}

/// `sessions.snapshots` — what can be rolled back to (`{ session_id? }`).
pub(super) async fn handle_sessions_snapshots(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    let checkpoints = state.infra.checkpoints.list(&session_id).await;
    let open = state.infra.checkpoints.open(&session_id).await;
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": session_id,
            // Which checkpoint is recording. `None` means writes are not being
            // kept, which is the ordinary state.
            "recording": open,
            "snapshots": checkpoints
                .iter()
                .map(|checkpoint| serde_json::json!({
                    "id": checkpoint.id,
                    "label": checkpoint.label,
                    "created_at": checkpoint.created_at.to_rfc3339(),
                    "files": checkpoint.files,
                }))
                .collect::<Vec<_>>(),
        }),
    )
}

/// `sessions.rollback` — put this session's files back (`{ session_id?, id? }`).
///
/// With no `id`, the newest checkpoint. The files are restored from the
/// pre-images taken while it was open, and every checkpoint from that point on
/// is dropped: they describe a state that no longer applies.
pub(super) async fn handle_sessions_rollback(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RollbackParams {
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        #[serde(default)]
        id: Option<String>,
    }

    let params: RollbackParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    let checkpoints = state.infra.checkpoints.list(&session_id).await;
    let id = match params.id.filter(|id| !id.is_empty()) {
        Some(id) => id,
        // The newest, because `/rollback` with no argument means "undo what I
        // have been doing since the last snapshot", which is the common case.
        None => match checkpoints.first() {
            Some(checkpoint) => checkpoint.id.clone(),
            None => {
                return WsResponse::err(
                    &req.id,
                    "NOT_FOUND",
                    format!("session '{session_id}' has no snapshots to roll back to"),
                )
            }
        },
    };

    match state.infra.checkpoints.rollback(&session_id, &id).await {
        Ok(report) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "id": id,
                "restored": report.restored,
                "removed": report.removed,
                "dropped": report.dropped,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "ROLLBACK_FAILED", e.to_string()),
    }
}

/// `sessions.branch` — continue this conversation down a second path
/// (`{ session_id?, name? }`).
///
/// The branch is a new session holding what this one has so far, so exploring an
/// alternative does not disturb the original. The copying happens in the agent
/// (which owns the turns and the message window) and is persisted as it goes, so
/// the branch is a conversation like any other: it appears in `sessions.list`,
/// `chat.history` shows it, and `/resume` switches to it.
pub(super) async fn handle_sessions_branch(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct BranchParams {
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    }

    let params: BranchParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    let branch_id = match params
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(name) => format!("{session_id}:{name}"),
        None => format!("{session_id}:fork-{}", Utc::now().format("%Y%m%d-%H%M%S")),
    };
    if let Some(ref store) = state.agents.store {
        match store.load_session(&branch_id).await {
            Ok(Some(_)) => {
                return WsResponse::err(
                    &req.id,
                    "ALREADY_EXISTS",
                    format!("session '{branch_id}' already exists"),
                )
            }
            Ok(None) => {}
            Err(e) => {
                return WsResponse::err(
                    &req.id,
                    "BRANCH_FAILED",
                    format!("could not check whether '{branch_id}' exists: {e}"),
                )
            }
        }
    }

    let agent_id = state
        .agents
        .router
        .resolve_by_session(&session_id)
        .await
        .agent_id;
    let handle = {
        let agents = state.agents.agents.read().await;
        agents.get(&agent_id).cloned()
    };
    let Some(handle) = handle else {
        return WsResponse::err(
            &req.id,
            "NOT_FOUND",
            format!("session '{session_id}' is not attached to a running agent"),
        );
    };

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    if handle
        .query_tx
        .send(crate::gateway::runtime::AgentQuery::ForkConversation {
            from: session_id.clone(),
            to: branch_id.clone(),
            response_tx,
        })
        .await
        .is_err()
    {
        return WsResponse::err(
            &req.id,
            "AGENT_UNAVAILABLE",
            format!("agent '{agent_id}' is no longer running"),
        );
    }
    let turns = match response_rx.await {
        Ok(Ok(turns)) => turns,
        Ok(Err(e)) => return WsResponse::err(&req.id, "BRANCH_FAILED", e.to_string()),
        Err(e) => {
            warn!("Agent {agent_id} did not answer a branch request: {e}");
            return WsResponse::err(
                &req.id,
                "AGENT_UNAVAILABLE",
                format!("agent '{agent_id}' did not answer"),
            );
        }
    };

    // The session row is what makes the branch findable; the turns the agent
    // copied are already in its history.
    let metadata =
        crate::agent::session_store::SessionMetadata::new(&branch_id, &agent_id, "ws", "ws");
    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.save_session(&branch_id, &metadata, "{}").await {
            return WsResponse::err(
                &req.id,
                "BRANCH_FAILED",
                format!("the branch was copied but its session row could not be written: {e}"),
            );
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": branch_id,
            "branch_of": session_id,
            "turns": turns,
        }),
    )
}

/// `sessions.export` — write a session's conversation to a file
/// (`{ session_id?, format? }`).
///
/// The conversation comes from the **session store**, not from the transcript
/// buffer this process happens to hold: the store is the durable record, and a
/// session untouched since the last start exports exactly like one being talked
/// to right now. That is also what makes this the export path rather than a
/// third one — the CLI's exporter and the `/export-session` command each had
/// their own idea of which formats existed and where the messages came from.
pub(super) async fn handle_sessions_export(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ExportParams {
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        #[serde(default)]
        format: Option<String>,
    }

    let params: ExportParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };
    let asked = params.format.as_deref().unwrap_or("md");
    let Some(format) = crate::gateway::session_export::parse_format(asked) else {
        return WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            format!(
                "unknown format '{asked}': expected {}",
                crate::gateway::session_export::FORMATS
            ),
        );
    };

    match crate::gateway::session_export::export_session(state, &session_id, format).await {
        Ok(path) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "format": format.to_string(),
                "path": path.display().to_string(),
                "file_name": path.file_name().map(|n| n.to_string_lossy().to_string()),
            }),
        ),
        Err(crate::gateway::session_export::ExportError::Empty) => WsResponse::err(
            &req.id,
            "NOT_FOUND",
            format!("session '{session_id}' has no messages to export"),
        ),
        Err(crate::gateway::session_export::ExportError::Failed(e)) => {
            WsResponse::err(&req.id, "EXPORT_FAILED", e)
        }
    }
}

/// `sessions.import` — read a saved conversation into a session
/// (`{ session_id?, file }`).
///
/// The other half of `sessions.export`, and deliberately narrower: only the
/// JSON rendering round-trips, because the others are documents. The messages
/// are appended to the session's history, where the next turn and the history
/// view will find them.
pub(super) async fn handle_sessions_import(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ImportParams {
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        file: String,
    }

    let params: ImportParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    match crate::gateway::session_export::import_session(state, &session_id, &params.file).await {
        Ok(imported) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "messages": imported.messages,
                "from": imported.from,
                "title": imported.title,
            }),
        ),
        Err(crate::gateway::session_export::ExportError::Empty) => WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            format!("'{}' holds no messages", params.file),
        ),
        Err(crate::gateway::session_export::ExportError::Failed(e)) => {
            WsResponse::err(&req.id, "IMPORT_FAILED", e)
        }
    }
}

/// `sessions.undo` — walk a session back over its last turn, or several
/// (`{ session_id?, count? }`).
///
/// The undo itself is the agent's, and it stays there: the request goes down the
/// agent's query channel, so an undo is serialised with the turns it is undoing
/// rather than racing the loop that is running one. What this adds is a way in —
/// `AgentQuery::UndoLastTurn` had no caller — and the loop for `/undo 5`, which
/// stops at the first turn there was nothing to undo.
pub(super) async fn handle_sessions_undo(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct UndoParams {
        /// Read via [`session_id_for`]; accepted here so the field is not
        /// rejected as unknown.
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        #[serde(default)]
        count: Option<u32>,
    }

    let params: UndoParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // A session the caller did not name is the one it is watching, which is what
    // makes `/undo` in the TUI a command with no arguments at all.
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    let count = params.count.unwrap_or(1).clamp(1, 1000);

    let agent_id = state
        .agents
        .router
        .resolve_by_session(&session_id)
        .await
        .agent_id;
    let handle = {
        let agents = state.agents.agents.read().await;
        agents.get(&agent_id).cloned()
    };
    let Some(handle) = handle else {
        return WsResponse::err(
            &req.id,
            "NOT_FOUND",
            format!("session '{session_id}' is not attached to a running agent"),
        );
    };

    let mut undone = 0u32;
    for _ in 0..count {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        if handle
            .query_tx
            .send(crate::gateway::runtime::AgentQuery::UndoLastTurn {
                conv_id: session_id.clone(),
                response_tx,
            })
            .await
            .is_err()
        {
            // The agent loop is gone: report what did happen rather than
            // pretending the rest of the undo did too.
            break;
        }
        match response_rx.await {
            Ok(true) => undone += 1,
            // Nothing left to undo, or the agent could not answer: either way
            // there is no point asking again.
            Ok(false) => break,
            Err(e) => {
                warn!("Agent {agent_id} did not answer an undo request: {e}");
                break;
            }
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "undone",
            "session_id": session_id,
            "undone": undone,
            "requested": count,
        }),
    )
}

pub(super) async fn handle_sessions_rename(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RenameParams {
        session_id: String,
        name: String,
    }

    let params: RenameParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let trimmed = params.name.trim();
    if trimmed.is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "session name cannot be empty");
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.set_session_name(&params.session_id, trimmed).await {
            warn!("Failed to rename session {}: {}", params.session_id, e);
            return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string());
        }
    }

    // Broadcast the rename event so all connected clients update immediately.
    if let Err(e) = state.events.tx.send(GatewayEvent::SessionRenamed {
        session_id: params.session_id.clone(),
        name: trimmed.to_string(),
    }) {
        tracing::debug!("No receivers for SessionRenamed event: {}", e);
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "renamed",
            "session_id": params.session_id,
            "name": trimmed,
        }),
    )
}

pub(super) async fn handle_sessions_set_pinned(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SetPinnedParams {
        session_id: String,
        pinned: bool,
    }

    let params: SetPinnedParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store
            .set_session_pinned(&params.session_id, params.pinned)
            .await
        {
            warn!("Failed to set pinned status for session {}: {}", params.session_id, e);
            return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string());
        }
    }

    if let Err(e) = state.events.tx.send(GatewayEvent::SessionPinned {
        session_id: params.session_id.clone(),
        pinned: params.pinned,
    }) {
        tracing::debug!("No receivers for SessionPinned event: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "ok" }))
}

pub(super) async fn handle_sessions_set_model(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SetModelParams {
        session_id: String,
        #[serde(default)]
        model: Option<String>,
    }

    let params: SetModelParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // Treat an empty string the same as clearing the pin.
    let model = params.model.filter(|m| !m.is_empty());

    // Validate the model reference against the model router when pinning.
    // Accepts bare ids and provider-qualified cloud refs (`cloud/<id>`).
    if let Some(ref m) = model {
        if state
            .infra
            .model_router
            .provider_for_model(m)
            .await
            .is_none()
        {
            return WsResponse::err(&req.id, "MODEL_NOT_FOUND", format!("Unknown model: {}", m));
        }
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store
            .set_session_model(&params.session_id, model.as_deref())
            .await
        {
            warn!("Failed to set model for session {}: {}", params.session_id, e);
            return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string());
        }
    }

    if let Err(e) = state.events.tx.send(GatewayEvent::SessionModelChanged {
        session_id: params.session_id.clone(),
        model: model.clone(),
    }) {
        tracing::debug!("No receivers for SessionModelChanged event: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "ok" }))
}

/// `sessions.set_mode {session_id, mode}` — set this conversation's
/// permission-mode override. The override is deliberately ephemeral: it dies
/// with the daemon, and the gateway falls back to the configured
/// `[permissions].mode` default. `bypass` is refused unless the config
/// turned `[permissions].allow_bypass` on.
pub(super) async fn handle_sessions_set_mode(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SetModeParams {
        session_id: String,
        mode: String,
    }

    let params: SetModeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let Some(mode) = crate::tools::PermissionMode::parse(&params.mode) else {
        return WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            "unknown mode; expected default|accept_edits|plan|bypass",
        );
    };
    if mode == crate::tools::PermissionMode::Bypass
        && !state.tools.registry.permissions().allow_bypass()
    {
        return WsResponse::err(
            &req.id,
            "BYPASS_DISABLED",
            "bypass requires [permissions].allow_bypass = true in gateway config",
        );
    }

    state
        .tools
        .registry
        .permissions()
        .set_session_mode(&params.session_id, Some(mode));

    if let Err(e) = state.events.tx.send(GatewayEvent::SessionModeChanged {
        session_id: params.session_id.clone(),
        mode: mode.as_str().to_string(),
    }) {
        tracing::debug!("No receivers for SessionModeChanged event: {}", e);
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "ok",
            "session_id": params.session_id,
            "mode": mode.as_str(),
        }),
    )
}

pub(super) async fn handle_sessions_reset(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ResetParams {
        session_id: String,
    }

    let params: ResetParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = state.agents.acp.cancel(params.session_id.clone()).await {
        warn!("Failed to cancel session {} during reset: {}", params.session_id, e);
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.delete_session(&params.session_id).await {
            warn!("Failed to delete session {} during reset: {}", params.session_id, e);
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "reset",
            "session_id": params.session_id,
        }),
    )
}

pub(super) async fn handle_sessions_subscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SubscribeParams {
        session_ids: Vec<String>,
    }

    let params: SubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx
        .send(WsCommand::Subscribe(params.session_ids.clone()))
        .await
    {
        warn!("Failed to send subscribe command: {}", e);
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "subscribed": params.session_ids,
        }),
    )
}

pub(super) async fn handle_sessions_unsubscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct UnsubscribeParams {
        session_ids: Vec<String>,
    }

    let params: UnsubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx
        .send(WsCommand::Unsubscribe(params.session_ids.clone()))
        .await
    {
        warn!("Failed to send unsubscribe command: {}", e);
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "unsubscribed": params.session_ids,
        }),
    )
}

pub(super) async fn handle_legacy_subscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct LegacySubscribeParams {
        session_ids: Vec<String>,
    }

    let params: LegacySubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx.send(WsCommand::Subscribe(params.session_ids)).await {
        warn!("Failed to send legacy subscribe command: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "subscribed" }))
}

pub(super) async fn handle_legacy_unsubscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct LegacyUnsubscribeParams {
        session_ids: Vec<String>,
    }

    let params: LegacyUnsubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx
        .send(WsCommand::Unsubscribe(params.session_ids))
        .await
    {
        warn!("Failed to send legacy unsubscribe command: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "unsubscribed" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::{
        make_test_conn, make_test_ctx, make_test_state, make_test_state_with_store,
    };
    use crate::gateway::GatewayConfig;
    use crate::model_router::{ProviderConfig, ProviderType};

    fn req(id: &str, method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params: Some(params),
        }
    }

    async fn register_model(state: &GatewayState, model_id: &str) {
        let config = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec![model_id.to_string()],
            default_model: model_id.to_string(),
            api_key: "test-key".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        state
            .infra
            .model_router
            .add_provider("openai", config)
            .await
            .expect("register provider");
    }

    #[tokio::test]
    async fn set_model_rejects_unknown_model() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_set_model(
            &req(
                "r1",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "no-such-model" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("MODEL_NOT_FOUND"));
    }

    /// A provider-qualified cloud ref (`cloud/<id>`) is accepted when the cloud
    /// proxy serves the bare id; an unserved ref is still rejected.
    #[tokio::test]
    async fn set_model_accepts_qualified_cloud_ref() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let cloud = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec!["deepseek-v4-pro".to_string()],
            default_model: "deepseek-v4-pro".to_string(),
            api_key: "test-key".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        state
            .infra
            .model_router
            .add_provider("cloud", cloud)
            .await
            .expect("register cloud");
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_set_model(
            &req(
                "r1",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "cloud/deepseek-v4-pro" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok, "qualified cloud ref should be accepted: {:?}", res.error);

        let res = handle_sessions_set_model(
            &req(
                "r2",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "cloud/ghost" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("MODEL_NOT_FOUND"));
    }

    #[tokio::test]
    async fn set_model_roundtrip_with_event_and_list() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        register_model(&state, "gpt-4o").await;
        let conn = make_test_conn(&["write"]);
        let mut rx = state.events.tx.subscribe();

        // Pin to the registered concrete model ID.
        let res = handle_sessions_set_model(
            &req(
                "r1",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "gpt-4o" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        match rx.try_recv() {
            Ok(GatewayEvent::SessionModelChanged { session_id, model }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(model.as_deref(), Some("gpt-4o"));
            }
            other => panic!("expected SessionModelChanged event, got {:?}", other.is_ok()),
        }

        // sessions.list surfaces the pin.
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let s1 = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("s1"))
            .expect("s1 should be listed");
        assert_eq!(s1.get("model").and_then(|v| v.as_str()), Some("gpt-4o"));

        // Clearing the pin stores NULL and emits the event with model: null.
        let res = handle_sessions_set_model(
            &req(
                "r2",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": null }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        match rx.try_recv() {
            Ok(GatewayEvent::SessionModelChanged { model, .. }) => assert_eq!(model, None),
            other => panic!("expected SessionModelChanged event, got {:?}", other.is_ok()),
        }
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let s1 = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("s1"))
            .expect("s1 should still be listed");
        assert!(s1.get("model").unwrap().is_null());
    }

    #[tokio::test]
    async fn set_model_empty_string_clears_pin() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        register_model(&state, "gpt-4o").await;
        let conn = make_test_conn(&["write"]);

        for value in [serde_json::json!("gpt-4o"), serde_json::json!("")] {
            let res = handle_sessions_set_model(
                &req(
                    "r",
                    "sessions.set_model",
                    serde_json::json!({ "session_id": "s1", "model": value }),
                ),
                &conn,
                &state,
            )
            .await;
            assert!(res.ok);
        }
        let store = state.agents.store.as_ref().unwrap();
        let loaded = store.load_session("s1").await.unwrap().unwrap();
        assert_eq!(loaded.metadata.model, None);
    }

    #[tokio::test]
    async fn create_persists_agent_binding() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_create(
            &req(
                "c",
                "sessions.create",
                serde_json::json!({ "session_id": "s1", "agent_id": "secretary" }),
            ),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;
        assert!(res.ok);

        let store = state.agents.store.as_ref().unwrap();
        let loaded = store.load_session("s1").await.unwrap().unwrap();
        assert_eq!(loaded.metadata.agent_id, "secretary");
        assert_eq!(loaded.metadata.bound_agent_id.as_deref(), Some("secretary"));
    }

    #[tokio::test]
    async fn create_without_id_derives_from_channel_and_user() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        // Test conn has no client/user, so the id is "ws:anonymous".
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_create(
            &req("c", "sessions.create", serde_json::json!({})),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;
        assert!(res.ok);
        let payload = res.payload.unwrap();
        assert_eq!(payload.get("session_id").and_then(|v| v.as_str()), Some("ws:anonymous"));
    }

    #[tokio::test]
    async fn rename_roundtrip_and_empty_rejected() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        handle_sessions_create(
            &req("c", "sessions.create", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;

        let res = handle_sessions_rename(
            &req(
                "r",
                "sessions.rename",
                serde_json::json!({ "session_id": "s1", "name": "  My Chat  " }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        let store = state.agents.store.as_ref().unwrap();
        let loaded = store.load_session("s1").await.unwrap().unwrap();
        assert_eq!(loaded.metadata.name.as_deref(), Some("My Chat"));

        let res = handle_sessions_rename(
            &req(
                "r2",
                "sessions.rename",
                serde_json::json!({ "session_id": "s1", "name": "   " }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("INVALID_PARAMS"));
    }

    #[tokio::test]
    async fn set_pinned_roundtrip_and_event() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let mut rx = state.events.tx.subscribe();
        handle_sessions_create(
            &req("c", "sessions.create", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;

        let res = handle_sessions_set_pinned(
            &req(
                "p",
                "sessions.set_pinned",
                serde_json::json!({ "session_id": "s1", "pinned": true }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        match rx.try_recv() {
            Ok(GatewayEvent::SessionPinned { session_id, pinned }) => {
                assert_eq!(session_id, "s1");
                assert!(pinned);
            }
            other => panic!("expected SessionPinned event, got {:?}", other.is_ok()),
        }

        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let s1 = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("s1"))
            .unwrap();
        assert_eq!(s1.get("pinned").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn delete_and_reset_remove_session() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        for sid in ["s1", "s2"] {
            handle_sessions_create(
                &req("c", "sessions.create", serde_json::json!({ "session_id": sid })),
                &conn,
                &state,
                &make_test_ctx(),
            )
            .await;
        }

        let res = handle_sessions_delete(
            &req("d", "sessions.delete", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        let res = handle_sessions_reset(
            &req("x", "sessions.reset", serde_json::json!({ "session_id": "s2" })),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);

        let store = state.agents.store.as_ref().unwrap();
        assert!(store.load_session("s1").await.unwrap().is_none());
        assert!(store.load_session("s2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_without_store_falls_back_to_manager() {
        // No store wired: the handler falls back to the in-memory session
        // manager and returns an empty list rather than erroring.
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        assert!(res.ok);
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        assert!(sessions.is_empty());
    }
    #[tokio::test]
    async fn set_mode_updates_the_runtime_and_emits_the_event() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let mut rx = state.events.tx.subscribe();
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_set_mode(
            &req(
                "m",
                "sessions.set_mode",
                serde_json::json!({ "session_id": "s1", "mode": "plan" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok, "set_mode must succeed: {:?}", res.error);
        assert_eq!(res.payload.expect("payload")["mode"].as_str(), Some("plan"));
        assert_eq!(
            state.tools.registry.permissions().effective_mode("s1"),
            crate::tools::PermissionMode::Plan,
            "the session override reaches the gate's runtime"
        );

        // The wire event names the session and the mode.
        match rx.recv().await.expect("event") {
            GatewayEvent::SessionModeChanged { session_id, mode } => {
                assert_eq!(session_id, "s1");
                assert_eq!(mode, "plan");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_mode_rejects_an_unknown_mode() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_set_mode(
            &req(
                "m",
                "sessions.set_mode",
                serde_json::json!({ "session_id": "s1", "mode": "neon" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("INVALID_PARAMS"));
        assert_eq!(
            state.tools.registry.permissions().effective_mode("s1"),
            crate::tools::PermissionMode::Default,
            "the reject must not touch the runtime"
        );
    }

    #[tokio::test]
    async fn set_mode_bypass_is_refused_until_the_config_flag_turns_on() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        async fn call_bypass(state: &Arc<GatewayState>) -> WsResponse {
            handle_sessions_set_mode(
                &req(
                    "m",
                    "sessions.set_mode",
                    serde_json::json!({ "session_id": "s1", "mode": "bypass" }),
                ),
                &make_test_conn(&["write"]),
                state,
            )
            .await
        }

        let res = call_bypass(&state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("BYPASS_DISABLED"));
        assert_eq!(
            state.tools.registry.permissions().effective_mode("s1"),
            crate::tools::PermissionMode::Default,
            "a refused bypass must not land in the runtime"
        );

        // Flip the config flag (through the typed arm, which also reloads the
        // runtime) and the same call succeeds.
        let res = super::config_ws::handle_config_set(
            &req(
                "c",
                "config.set",
                serde_json::json!({ "path": "permissions.allow_bypass", "value": true }),
            ),
            &state,
        )
        .await;
        assert!(res.ok, "allow_bypass must set: {:?}", res.error);
        let res = call_bypass(&state).await;
        assert!(res.ok, "bypass works once the flag is on: {:?}", res.error);
        assert_eq!(
            state.tools.registry.permissions().effective_mode("s1"),
            crate::tools::PermissionMode::Bypass
        );
    }

    /// Build a state with one running agent whose query channel answers the
    /// `UndoLastTurn` queries from `answers`, in order — a stand-in for the
    /// agent loop, which is what really answers them. `GetPlan` is answered with
    /// no plan: these tests are about the undo path.
    async fn state_with_undoing_agent(answers: Vec<bool>) -> Arc<GatewayState> {
        // With a store, because two of these tests are about what reaches it.
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::mock::MockProvider::new());
        let tools = Arc::new(crate::tools::ToolRegistry::new());
        let agent_config = crate::agent::AgentConfig::default();
        let agent = Arc::new(crate::agent::Agent::new(agent_config.clone(), provider, tools));

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (query_tx, mut query_rx) = tokio::sync::mpsc::channel::<crate::gateway::AgentQuery>(8);
        let handle = crate::gateway::AgentHandle {
            id: "agent-a".to_string(),
            config: agent_config.clone(),
            base_config: agent_config,
            tx,
            query_tx,
            busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent,
        };
        state
            .agents
            .agents
            .write()
            .await
            .insert("agent-a".to_string(), handle);

        tokio::spawn(async move {
            let mut answers = answers.into_iter();
            while let Some(query) = query_rx.recv().await {
                match query {
                    crate::gateway::AgentQuery::UndoLastTurn { response_tx, .. } => {
                        let answer = answers.next().unwrap_or(false);
                        let _ = response_tx.send(answer);
                    }
                    crate::gateway::AgentQuery::GetPlan { response_tx, .. } => {
                        let _ = response_tx.send(None);
                    }
                    // A fork of a conversation with no turns copies nothing, and
                    // that is what the real agent answers too.
                    crate::gateway::AgentQuery::ForkConversation { response_tx, .. } => {
                        let _ = response_tx.send(Ok(0));
                    }
                    // The handler only sends undo queries; anything else means
                    // this test has lost track of what it is testing.
                    _ => panic!("the handler sent a query this test does not answer"),
                }
            }
        });

        state
            .agents
            .router
            .bind_session(
                "s1",
                &crate::inbound::RouteResult {
                    agent_id: "agent-a".to_string(),
                    workspace_id: None,
                    persisted_binding: false,
                    is_fallback: false,
                },
            )
            .await;

        state
    }

    #[tokio::test]
    async fn undo_walks_back_and_stops_when_there_is_nothing_left() {
        // Three asks, two turns to give: the third answer is the agent saying
        // there is nothing left, and the loop must stop there rather than keep
        // asking for the rest of the count.
        let state = state_with_undoing_agent(vec![true, true, false]).await;
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_undo(
            &req("r1", "sessions.undo", serde_json::json!({ "session_id": "s1", "count": 5 })),
            &conn,
            &state,
        )
        .await;

        assert!(res.ok, "{:?}", res.error);
        let payload = res.payload.expect("payload");
        assert_eq!(payload["undone"], 2);
        assert_eq!(payload["requested"], 5);
        assert_eq!(payload["session_id"], "s1");
    }

    /// `/undo` with no argument is the common case, and a client watching one
    /// session should not have to name it.
    #[tokio::test]
    async fn undo_defaults_to_the_subscribed_session_and_one_turn() {
        let state = state_with_undoing_agent(vec![true]).await;
        let conn = make_test_conn(&["write"]);
        conn.write().await.subscriptions.push("s1".to_string());

        let res =
            handle_sessions_undo(&req("r1", "sessions.undo", serde_json::json!({})), &conn, &state)
                .await;

        assert!(res.ok, "{:?}", res.error);
        let payload = res.payload.expect("payload");
        assert_eq!(payload["undone"], 1);
        assert_eq!(payload["requested"], 1);
    }

    /// `sessions.plan` reaches the agent and reports what it says — including
    /// "nothing", which is the ordinary state of a conversation.
    #[tokio::test]
    async fn plan_reports_the_agents_answer_including_none() {
        let state = state_with_undoing_agent(vec![]).await;
        let conn = make_test_conn(&["read"]);

        let res = handle_sessions_plan(
            &req("r1", "sessions.plan", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;

        assert!(res.ok, "{:?}", res.error);
        let payload = res.payload.expect("payload");
        assert_eq!(payload["session_id"], "s1");
        assert!(payload["plan"].is_null(), "a conversation with no plan reports none");
    }

    /// The plan belongs to the agent, so a session with no running agent has no
    /// plan to report — and saying so beats reporting an empty one.
    #[tokio::test]
    async fn plan_reports_a_session_with_no_running_agent() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let conn = make_test_conn(&["read"]);

        let res = handle_sessions_plan(
            &req("r1", "sessions.plan", serde_json::json!({ "session_id": "nobody-home" })),
            &conn,
            &state,
        )
        .await;

        assert!(!res.ok);
        assert_eq!(res.error.expect("error").code, "NOT_FOUND");
    }

    /// Export reads the *stored* conversation, so a session nobody has touched
    /// in this process exports like any other — which is the whole reason it
    /// reads the store rather than the transcript buffer.
    #[tokio::test]
    async fn export_writes_the_stored_conversation_in_the_format_asked_for() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let store = state.agents.store.clone().expect("store");
        for (role, content) in [("user", "what is 2+2?"), ("assistant", "4")] {
            store
                .append_message(&crate::agent::session_store::AppendMessageParams {
                    session_id: "s1",
                    role,
                    content,
                    ..Default::default()
                })
                .await
                .expect("append");
        }
        let conn = make_test_conn(&["read"]);

        for (asked, expected_ext) in [("md", "md"), ("json", "json"), ("text", "txt")] {
            let res = handle_sessions_export(
                &req(
                    "r1",
                    "sessions.export",
                    serde_json::json!({ "session_id": "s1", "format": asked }),
                ),
                &conn,
                &state,
            )
            .await;
            assert!(res.ok, "{asked}: {:?}", res.error);
            let payload = res.payload.expect("payload");
            let path = payload["path"].as_str().expect("path").to_string();
            assert!(path.ends_with(expected_ext), "{asked} wrote {path}");

            let written = tokio::fs::read_to_string(&path)
                .await
                .expect("the file is there");
            assert!(written.contains("2+2"), "{asked} lost the question");
            assert!(written.contains('4'), "{asked} lost the answer");
            let _ = tokio::fs::remove_file(&path).await;
        }
    }

    #[tokio::test]
    async fn export_refuses_an_unknown_format_and_an_empty_session() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["read"]);

        let res = handle_sessions_export(
            &req(
                "r1",
                "sessions.export",
                serde_json::json!({ "session_id": "s1", "format": "pdf" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.expect("error").code, "INVALID_PARAMS");

        // A session with nothing in it has nothing to write, and saying so
        // beats producing an empty file.
        let res = handle_sessions_export(
            &req("r2", "sessions.export", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.expect("error").code, "NOT_FOUND");
    }

    /// Export then import is the round trip: what was written can be read back
    /// into another session, message for message.
    #[tokio::test]
    async fn export_then_import_round_trips_the_conversation() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let store = state.agents.store.clone().expect("store");
        for (role, content) in [("user", "keep this"), ("assistant", "kept")] {
            store
                .append_message(&crate::agent::session_store::AppendMessageParams {
                    session_id: "source",
                    role,
                    content,
                    ..Default::default()
                })
                .await
                .expect("append");
        }
        let conn = make_test_conn(&["read", "write"]);

        let exported = handle_sessions_export(
            &req(
                "r1",
                "sessions.export",
                serde_json::json!({ "session_id": "source", "format": "json" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(exported.ok, "{:?}", exported.error);
        let file = exported.payload.expect("payload")["file_name"]
            .as_str()
            .expect("file_name")
            .to_string();

        let imported = handle_sessions_import(
            &req(
                "r2",
                "sessions.import",
                serde_json::json!({ "session_id": "target", "file": file }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(imported.ok, "{:?}", imported.error);
        let payload = imported.payload.expect("payload");
        assert_eq!(payload["messages"], 2);
        assert_eq!(payload["from"], "source");

        // The messages are in the target session's history, in order.
        let messages = store
            .get_messages("target", 10, None)
            .await
            .expect("read back");
        let contents: Vec<String> = messages.into_iter().map(|m| m.2).collect();
        assert_eq!(contents, vec!["keep this".to_string(), "kept".to_string()]);

        let _ = tokio::fs::remove_file(state.infra.transcript_store.root_dir().join(&file)).await;
    }

    /// A rendering is a document, so it is refused rather than parsed on a
    /// guess — and a path is refused rather than read.
    #[tokio::test]
    async fn import_refuses_a_render_and_a_path() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_import(
            &req(
                "r1",
                "sessions.import",
                serde_json::json!({ "session_id": "s1", "file": "/etc/passwd" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        let error = res.error.expect("error");
        assert_eq!(error.code, "IMPORT_FAILED");
        assert!(error.message.contains("is a path"), "{}", error.message);

        let res = handle_sessions_import(
            &req(
                "r2",
                "sessions.import",
                serde_json::json!({ "session_id": "s1", "file": "conversation.md" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        let error = res.error.expect("error");
        assert!(
            error
                .message
                .contains("only the JSON rendering can be read back"),
            "{}",
            error.message
        );
    }

    /// Branching copies the conversation under a new id and writes it down: the
    /// branch is a session like any other, which is what makes it something you
    /// can go back to.
    #[tokio::test]
    async fn branch_copies_the_conversation_into_a_new_session() {
        let state = state_with_undoing_agent(vec![]).await;
        let store = state.agents.store.clone().expect("store");
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_branch(
            &req(
                "r1",
                "sessions.branch",
                serde_json::json!({ "session_id": "s1", "name": "alternative" }),
            ),
            &conn,
            &state,
        )
        .await;

        assert!(res.ok, "{:?}", res.error);
        let payload = res.payload.expect("payload");
        assert_eq!(payload["session_id"], "s1:alternative");
        assert_eq!(payload["branch_of"], "s1");
        // The stand-in agent copies nothing (it has no turns), and the branch is
        // still a session: the copy is the agent's, the row is this method's.
        assert!(
            store
                .load_session("s1:alternative")
                .await
                .expect("lookup")
                .is_some(),
            "the branch has a session row"
        );
    }

    /// A branch named after a session that exists is refused: two sessions with
    /// one id is not two branches, it is a collision.
    #[tokio::test]
    async fn branch_refuses_a_name_already_taken() {
        let state = state_with_undoing_agent(vec![]).await;
        let store = state.agents.store.clone().expect("store");
        store
            .save_session(
                "s1:alternative",
                &crate::agent::session_store::SessionMetadata::new(
                    "s1:alternative",
                    "a",
                    "ws",
                    "u",
                ),
                "{}",
            )
            .await
            .expect("seed");
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_branch(
            &req(
                "r1",
                "sessions.branch",
                serde_json::json!({ "session_id": "s1", "name": "alternative" }),
            ),
            &conn,
            &state,
        )
        .await;

        assert!(!res.ok);
        assert_eq!(res.error.expect("error").code, "ALREADY_EXISTS");
    }

    /// Snapshot, then roll back: the files a session's writes replaced come
    /// back, and the checkpoint is spent.
    #[tokio::test]
    async fn snapshot_then_rollback_restores_the_files() {
        let state = state_with_undoing_agent(vec![]).await;
        assert!(state.infra.checkpoints.list("s1").await.is_empty());
        let conn = make_test_conn(&["write"]);

        // A file the session "writes into" — the store keeps whatever absolute
        // path it is handed, so it does not have to be under the profile.
        let dir = std::env::temp_dir().join(format!("syscity_rollback_ws_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.expect("dir");
        let file = dir.join("notes.txt");
        tokio::fs::write(&file, "before").await.expect("write");

        let opened = handle_sessions_snapshot(
            &req(
                "r1",
                "sessions.snapshot",
                serde_json::json!({ "session_id": "s1", "label": "experiment" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(opened.ok, "{:?}", opened.error);
        let id = opened.payload.expect("payload")["id"]
            .as_str()
            .unwrap()
            .to_string();

        // The write the checkpoint exists to undo.
        state
            .infra
            .checkpoints
            .capture("s1", &file)
            .await
            .expect("capture");
        tokio::fs::write(&file, "after").await.expect("write");

        let listed = handle_sessions_snapshots(
            &req("r2", "sessions.snapshots", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;
        let payload = listed.payload.expect("payload");
        assert_eq!(payload["recording"].as_str(), Some(id.as_str()));
        assert_eq!(payload["snapshots"].as_array().unwrap().len(), 1);
        assert_eq!(payload["snapshots"][0]["files"], 1);

        let rolled = handle_sessions_rollback(
            &req("r3", "sessions.rollback", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;
        assert!(rolled.ok, "{:?}", rolled.error);
        let payload = rolled.payload.expect("payload");
        assert_eq!(payload["restored"], 1);
        assert_eq!(tokio::fs::read_to_string(&file).await.expect("read"), "before");

        // And there is nothing left to roll back to, which is what stops a
        // second `/rollback` from claiming to have done anything.
        let again = handle_sessions_rollback(
            &req("r4", "sessions.rollback", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;
        assert!(!again.ok);
        assert_eq!(again.error.expect("error").code, "NOT_FOUND");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn undo_reports_a_session_with_no_running_agent() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_undo(
            &req("r1", "sessions.undo", serde_json::json!({ "session_id": "nobody-home" })),
            &conn,
            &state,
        )
        .await;

        assert!(!res.ok);
        assert_eq!(res.error.expect("error").code, "NOT_FOUND");
    }

    /// Nothing to name the session with: refused with the reason rather than
    /// undone against whichever session happened to be first.
    #[tokio::test]
    async fn undo_refuses_when_no_session_can_be_determined() {
        let state = state_with_undoing_agent(vec![true]).await;
        // No subscriptions, and the caller named none.
        let conn = make_test_conn(&["write"]);

        let res =
            handle_sessions_undo(&req("r1", "sessions.undo", serde_json::json!({})), &conn, &state)
                .await;

        assert!(!res.ok);
        assert_eq!(res.error.expect("error").code, "INVALID_PARAMS");
    }
}
