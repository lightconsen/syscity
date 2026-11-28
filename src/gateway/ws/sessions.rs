//! Session list/create/delete/rename/pin/reset + subscribe/unsubscribe (incl. legacy).

use chrono::Utc;

use super::*;
pub(super) async fn handle_sessions_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    // A connection bound to one agent lists its own agent's conversations plus
    // any it opened with a shared agent (one with no `allowed_users`). Which
    // rows those are is decided below, against the router's binding — the same
    // authority every other session handler consults — rather than by a single
    // `agent_id` equality.
    let bound = ctx.bound_agent().map(str::to_string);
    // An unbound caller sees only the conversations attributed to them. A
    // surfaced list of someone else's session is worse than the agent case: the
    // row carries their history, and every click on it would then be refused
    // one session at a time. A single-identity deployment records no owner
    // (`shared` / `anonymous` are not people), so nothing matches and the
    // filter is skipped — its listing stays exactly what it was.
    //
    // A bound caller is a real principal too: two token holders talking to the
    // same shared agent must not see each other's conversations in it, so the
    // owner filter applies to them as well.
    let has_personal_identity = {
        let id = ctx.user_id();
        !id.is_empty()
            && !crate::memory::SHARED_IDENTITIES
                .iter()
                .any(|s| s.eq_ignore_ascii_case(id))
    };
    let user_filter = if has_personal_identity {
        Some(ctx.user_id())
    } else {
        None
    };
    // The bindings, when a bound caller needs them for the agent half of the
    // filter. Read once; a listing is one snapshot either way.
    let bindings = match &bound {
        Some(_) => state.agents.router.list_bindings().await,
        None => Default::default(),
    };
    // The personal agents (restricted by `allowed_users`, or named by an agent
    // token), read once: a bound caller keeps its own agent's rows and any row
    // whose agent is *not* in this set.
    let personal = if bound.is_some() {
        crate::gateway::ws::bound::personal_agents(state).await
    } else {
        Default::default()
    };
    // Whether a row belongs to an agent this caller may use. Unbound callers
    // see everything the store query returned (their gate is the owner
    // filter). A bound caller keeps its own agent's rows and any row whose
    // agent is shared (not personal); a row with no binding falls back to the
    // stored `agent_id`, and a row with neither is not this client's.
    let row_visible = |session_id: &str, stored_agent: &str| -> bool {
        let Some(bound) = bound.as_deref() else {
            return true;
        };
        let agent = bindings
            .get(session_id)
            .map(|(a, _)| a.as_str())
            .filter(|a| !a.is_empty())
            .unwrap_or(stored_agent);
        !agent.is_empty() && (agent == bound || !personal.contains(agent))
    };
    // The mode a session actually runs in: a per-session override when one was
    // set (see `sessions.set_mode`), otherwise the configured default. The
    // composer's mode picker reads it from here so a client that just
    // connected shows the truth rather than guessing "default".
    let mode_of = |session_id: &str| {
        state
            .tools
            .registry
            .permissions()
            .effective_mode(session_id)
            .as_str()
            .to_string()
    };
    // Which conversations have a turn in flight, from the gateway's own
    // record. A client that reloaded mid-turn has nothing local to go on.
    let running = state.agents.running_sessions().await;
    let sessions: Vec<serde_json::Value> = if let Some(ref store) = state.agents.store {
        match store
            .find_sessions_for_user(None, None, None, false, user_filter)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter(|meta| row_visible(&meta.session_id, &meta.agent_id))
                .map(|meta| {
                    let name = meta.name.unwrap_or_else(|| {
                        if meta.message_count == 0 {
                            "New Session".to_string()
                        } else {
                            meta.last_activity.format("%b %d %H:%M").to_string()
                        }
                    });
                    let permission_mode = mode_of(&meta.session_id);
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
                        "user_id": meta.user_id,
                        "permission_mode": permission_mode,
                        "running": running.contains(&meta.session_id),
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
        // With no store there is no `agent_id` column to filter on, so the
        // router's binding is consulted instead — the same authority
        // `session_usable` uses. Ids are compared as bindings, never as
        // substrings: an agent named `alice` must not match `ws:alice-other`.
        let mgr = state.agents.manager.read().await;
        mgr.list_sessions()
            .await
            .into_iter()
            .filter(|id| row_visible(id, ""))
            .map(|id| {
                let permission_mode = mode_of(&id);
                serde_json::json!({
                    "session_id": id,
                    "name": id,
                    "permission_mode": permission_mode,
                    "running": running.contains(&id),
                })
            })
            .collect()
    };

    WsResponse::ok(&req.id, serde_json::json!({ "sessions": sessions }))
}

/// `sessions.status { session_id }` — is this conversation's turn still
/// running? (`{ session_id, running, awaiting_approval }`)
///
/// The gateway's own answer, not the client's memory of it. A reloaded page,
/// a reconnected socket, a turn started from the CLI or a channel, and a turn
/// that survived the client going away all leave a client with no local run
/// state — and a live view that shows no sign of work is indistinguishable
/// from one that stopped. This is what lets it say the truth instead.
///
/// `awaiting_approval` is part of the same question: a turn parked on a human
/// is running (nothing has ended), and the client can say so rather than
/// showing a bare spinner for an answer it is the one who must give.
pub(super) async fn handle_sessions_status(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct StatusParams {
        session_id: String,
    }

    let params: StatusParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let running = state.agents.is_turn_running(&params.session_id).await;

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": params.session_id,
            "running": running,
            // Whether a pending approval names this conversation — the client
            // has the full request from `approval.required` / `approvals.list`;
            // this only says whether the running turn is waiting on one.
            "awaiting_approval": if running {
                state
                    .tools
                    .approval_queue
                    .list_pending(crate::tools::approval::ApprovalFilter::default())
                    .await
                    .iter()
                    .any(|pa| pa.session_id.as_deref() == Some(params.session_id.as_str()))
            } else {
                false
            },
        }),
    )
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

    // A connection bound to one agent creates sessions for *that* agent. An
    // explicit `agent_id` that contradicts the binding was refused already; an
    // omitted one defaults here rather than falling through to the router's
    // global default, which would put an agent the caller may not use at the
    // top of their session list.
    let requested_agent = params
        .agent_id
        .clone()
        .or_else(|| ctx.bound_agent().map(str::to_string));

    // Authorize before anything is written. Refusing after the record exists
    // leaves a session in the caller's list that the router will then reject
    // on every message — and, worse, one they could see and try to use.
    if let Some(agent_id) = requested_agent.as_deref() {
        if !agent_id.is_empty() && !state.agents.router.allows_principal(agent_id, &user).await {
            warn!(
                "Refusing to create session {} bound to agent '{}' for user {}",
                session_id, agent_id, user
            );
            return WsResponse::err(
                &req.id,
                "FORBIDDEN",
                format!("Agent '{agent_id}' is not available to this user"),
            );
        }
    }

    {
        let mut mgr = state.agents.manager.write().await;
        mgr.create_session(session_id.clone());
    }

    let mut metadata = crate::agent::session_store::SessionMetadata::new(
        &session_id,
        requested_agent.as_deref().unwrap_or(""),
        &channel,
        &user,
    )
    .with_user_id(Some(&user));
    metadata.bound_agent_id = requested_agent.clone();

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.save_session(&session_id, &metadata, "{}").await {
            warn!("Failed to save session {}: {}", session_id, e);
        }
    }

    // Bind the session to the requested agent so future messages route there.
    if let Some(agent_id) = requested_agent {
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

/// `sessions.handoff` — write this session out as a bundle another gateway can
/// take (`{ session_id?, file? }`).
///
/// The bundle is the conversation plus the session state that is not in the
/// messages: its name, its model, and the plan it was working through. What it
/// is not is a live move — nothing about this session changes, and the receiving
/// gateway applies it to a session of its own.
pub(super) async fn handle_sessions_handoff(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct HandoffParams {
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        #[serde(default)]
        file: Option<String>,
    }

    let params: HandoffParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    // The plan is the agent's, so it is asked for rather than read here — and a
    // session whose agent is gone still hands off, without one.
    let plan = match agent_handle(state, &session_id).await {
        Some(handle) => match ask_agent(&handle, &session_id).await {
            Ok(plan) => plan,
            Err(e) => {
                tracing::debug!("Handing off {session_id} without its plan: {e}");
                None
            }
        },
        None => None,
    };

    match crate::gateway::session_export::write_handoff(
        state,
        &session_id,
        params.file.as_deref(),
        plan,
    )
    .await
    {
        Ok((path, bundle)) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "file_name": path.file_name().map(|n| n.to_string_lossy().to_string()),
                "path": path.display().to_string(),
                "messages": bundle.messages.len(),
                "plan_steps": bundle.plan.as_ref().and_then(|p| p.get("tasks"))
                    .and_then(|t| t.as_array()).map(|t| t.len()).unwrap_or(0),
            }),
        ),
        Err(crate::gateway::session_export::ExportError::Empty) => WsResponse::err(
            &req.id,
            "NOT_FOUND",
            format!("session '{session_id}' has no messages to hand off"),
        ),
        Err(crate::gateway::session_export::ExportError::Failed(e)) => {
            WsResponse::err(&req.id, "HANDOFF_FAILED", e)
        }
    }
}

/// `sessions.handoff_take` — apply a bundle to this session
/// (`{ session_id?, file }`).
pub(super) async fn handle_sessions_handoff_take(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct TakeParams {
        #[serde(default)]
        #[allow(dead_code)]
        session_id: Option<String>,
        file: String,
    }

    let params: TakeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let session_id = match session_id_for(req, conn, state).await {
        Ok(id) => id,
        Err(res) => return res,
    };

    match crate::gateway::session_export::take_handoff(state, &session_id, &params.file).await {
        Ok(taken) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "session_id": session_id,
                "messages": taken.messages,
                "from_session": taken.from_session,
                "applied": taken.applied,
                // Not applied, and said so: a plan belongs to the agent it is
                // running on, which builds one from the conversation.
                "plan_steps": taken.plan_steps,
            }),
        ),
        Err(crate::gateway::session_export::ExportError::Empty) => WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            format!("'{}' carries no messages", params.file),
        ),
        Err(crate::gateway::session_export::ExportError::Failed(e)) => {
            WsResponse::err(&req.id, "HANDOFF_FAILED", e)
        }
    }
}

/// The running agent for a session, if there is one.
async fn agent_handle(
    state: &Arc<GatewayState>,
    session_id: &str,
) -> Option<crate::gateway::AgentHandle> {
    let agent_id = state
        .agents
        .router
        .resolve_by_session(session_id)
        .await
        .agent_id;
    let agents = state.agents.agents.read().await;
    agents.get(&agent_id).cloned()
}

/// Ask an agent for a conversation's plan.
async fn ask_agent(
    handle: &crate::gateway::AgentHandle,
    session_id: &str,
) -> crate::Result<Option<serde_json::Value>> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    handle
        .query_tx
        .send(crate::gateway::runtime::AgentQuery::GetPlan {
            conv_id: session_id.to_string(),
            response_tx,
        })
        .await
        .map_err(|e| {
            crate::error::SyscityError::Internal(format!("the agent is no longer running: {e}"))
        })?;
    let plan = response_rx.await.map_err(|e| {
        crate::error::SyscityError::Internal(format!("the agent did not answer: {e}"))
    })?;
    Ok(plan.and_then(|plan| serde_json::to_value(plan).ok()))
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

/// `sessions.get_mode {session_id}` — the mode this conversation actually runs
/// in: its override if one was set, else the configured default.
///
/// `sessions.list` carries the same field, but only for sessions the store
/// knows about. A conversation that exists client-side and in the override map
/// but not yet in the store (the composer's session, before its first message)
/// is exactly the case a client needs to restore after a reload, so it gets a
/// read of its own rather than a scan of the list.
pub(super) async fn handle_sessions_get_mode(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct GetModeParams {
        session_id: String,
    }

    let params: GetModeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let permissions = state.tools.registry.permissions();
    let mode = permissions.effective_mode(&params.session_id);

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": params.session_id,
            "mode": mode.as_str(),
            // Lets the picker grey out what the gateway would refuse rather
            // than offering a mode that comes back as an error.
            "allow_bypass": permissions.allow_bypass(),
        }),
    )
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

    /// The `session_id`s of a `sessions.list` payload, sorted.
    fn listed_session_ids(res: &WsResponse) -> Vec<String> {
        let mut ids: Vec<String> = res
            .payload
            .as_ref()
            .and_then(|p| p.get("sessions"))
            .and_then(|s| s.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|s| {
                        s.get("session_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
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
        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &make_test_ctx(),
        )
        .await;
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
        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &make_test_ctx(),
        )
        .await;
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
    async fn create_refused_for_another_persons_agent_leaves_nothing_behind() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        state
            .agents
            .router
            .set_agent_principals("bob-bot", vec!["bob".to_string()])
            .await;
        let conn = make_test_conn(&["write"]);
        let alice = crate::security::request_context::RequestContext::new(
            "alice",
            crate::security::request_context::AuthSource::Session,
        );

        let res = handle_sessions_create(
            &req(
                "c",
                "sessions.create",
                serde_json::json!({ "session_id": "hijack", "agent_id": "bob-bot" }),
            ),
            &conn,
            &state,
            &alice,
        )
        .await;
        assert!(!res.ok, "creating on bob's agent must be refused: {res:?}");
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("FORBIDDEN"));

        // The refusal must come before the write: a session row left behind
        // would sit in alice's list and refuse every message she sends to it.
        let store = state.agents.store.as_ref().unwrap();
        assert!(
            store.load_session("hijack").await.unwrap().is_none(),
            "a refused session must not be persisted"
        );
        assert!(
            !state
                .agents
                .manager
                .read()
                .await
                .get_session("hijack")
                .is_some(),
            "a refused session must not be registered with the session manager"
        );
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

        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &make_test_ctx(),
        )
        .await;
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

    /// With no session store the binding has to come from the router, and it
    /// has to be compared as a binding, not as a substring: an agent named
    /// `alice` must not match a session id that merely contains "alice". A
    /// session with no binding is not this client's either — there is no agent
    /// to vouch for it.
    #[tokio::test]
    async fn sessions_list_without_a_store_narrows_to_the_router_binding() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        assert!(state.agents.store.is_none(), "this test is about the no-store path");
        {
            let mut mgr = state.agents.manager.write().await;
            mgr.create_session("ws:alice".to_string());
            mgr.create_session("ws:alice-other".to_string());
        }
        let route = crate::inbound::RouteResult {
            agent_id: "alice".to_string(),
            workspace_id: None,
            persisted_binding: true,
            is_fallback: false,
        };
        state.agents.router.bind_session("ws:alice", &route).await;

        let ctx = make_test_ctx().with_bound_agent(Some("alice"));
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state, &ctx)
                .await;
        let listed = listed_session_ids(&res);
        assert_eq!(
            listed,
            vec!["ws:alice".to_string()],
            "the bound agent sees its own binding and nothing a substring match would catch"
        );
    }

    /// The listing has to agree with what `check_params` accepts. A bound
    /// connection may open a conversation with a shared agent (no
    /// `allowed_users`), so that conversation must appear in its list —
    /// otherwise the user creates a session, sends a message, and the sidebar
    /// pretends it does not exist.
    #[tokio::test]
    async fn sessions_list_shows_shared_agent_conversations_to_a_bound_caller() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let store = state.agents.store.as_ref().unwrap();
        for (session, agent) in [
            ("own-1", "secretary-alice"),
            ("shared-1", "secretary"),
            ("personal-1", "secretary-bob"),
        ] {
            let meta =
                crate::agent::session_store::SessionMetadata::new(session, agent, "ws", "alice")
                    .with_user_id(Some("alice"));
            store.save_session(session, &meta, "{}").await.unwrap();
            let route = crate::inbound::RouteResult {
                agent_id: agent.to_string(),
                workspace_id: None,
                persisted_binding: true,
                is_fallback: false,
            };
            state.agents.router.bind_session(session, &route).await;
        }
        // `secretary-bob` is personal to bob; `secretary` and
        // `secretary-alice` are not restricted at all.
        state
            .agents
            .router
            .set_agent_principals("secretary-bob", vec!["bob".to_string()])
            .await;

        let ctx =
            RequestContext::new("alice", crate::security::request_context::AuthSource::Session)
                .with_bound_agent(Some("secretary-alice"));
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state, &ctx)
                .await;
        assert!(res.ok, "list failed: {:?}", res.error);
        assert_eq!(
            listed_session_ids(&res),
            vec!["own-1", "shared-1"],
            "own agent + shared agent; a personal agent's conversation stays out"
        );
    }

    /// Two token holders sharing the same secretary must not see each other's
    /// conversations in it: the owner filter applies to a bound caller too.
    #[tokio::test]
    async fn sessions_list_hides_another_users_shared_agent_conversation() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let store = state.agents.store.as_ref().unwrap();
        for (session, owner) in [("alice-sec", "alice"), ("bob-sec", "bob")] {
            let meta = crate::agent::session_store::SessionMetadata::new(
                session,
                "secretary",
                "ws",
                owner,
            )
            .with_user_id(Some(owner));
            store.save_session(session, &meta, "{}").await.unwrap();
        }

        let ctx =
            RequestContext::new("alice", crate::security::request_context::AuthSource::Session)
                .with_bound_agent(Some("secretary-alice"));
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state, &ctx)
                .await;
        assert!(res.ok, "list failed: {:?}", res.error);
        assert_eq!(listed_session_ids(&res), vec!["alice-sec"]);
    }

    /// A caller with a per-user identity sees only the conversations attributed
    /// to them. A session belongs to the person whose history it carries, and
    /// every click into someone else's is one refused action at a time.
    #[tokio::test]
    async fn sessions_list_filters_by_the_callers_user() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let store = state.agents.store.as_ref().unwrap();
        for (session, owner) in [
            ("alice-1", Some("alice")),
            ("alice-2", Some("alice")),
            ("bob-1", Some("bob")),
            ("unowned", None),
        ] {
            let mut meta = crate::agent::session_store::SessionMetadata::new(
                session,
                "",
                "ws",
                owner.unwrap_or(""),
            );
            meta = meta.with_user_id(owner);
            store.save_session(session, &meta, "{}").await.unwrap();
        }

        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &RequestContext::new("alice", crate::security::request_context::AuthSource::Session),
        )
        .await;
        assert!(res.ok, "list failed: {:?}", res.error);
        let listed: Vec<String> = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|s| {
                s.get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        let mut listed = listed;
        listed.sort();
        assert_eq!(listed, vec!["alice-1", "alice-2"]);
    }

    /// A single-identity deployment records no owner (`shared` / `anonymous`
    /// are not people), so listing must not suddenly hide everything from it.
    /// Filtering is skipped for such a caller.
    #[tokio::test]
    async fn sessions_list_does_not_filter_for_a_shared_identity() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let store = state.agents.store.as_ref().unwrap();
        for (session, owner) in [("one", Some("alice")), ("two", Some("bob"))] {
            let meta =
                crate::agent::session_store::SessionMetadata::new(session, "", "ws", "shared")
                    .with_user_id(owner);
            store.save_session(session, &meta, "{}").await.unwrap();
        }

        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &make_test_ctx(),
        )
        .await;
        let listed = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default()
            .len();
        assert_eq!(listed, 2, "a shared identity sees the whole deployment's list");
    }

    /// A session created by a real principal is recorded with that owner, and
    /// the same principal's later list finds it.
    #[tokio::test]
    async fn create_records_the_owner_and_list_finds_it() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let ctx =
            RequestContext::new("alice", crate::security::request_context::AuthSource::Session);
        let res = handle_sessions_create(
            &req("c", "sessions.create", serde_json::json!({ "session_id": "mine" })),
            &conn,
            &state,
            &ctx,
        )
        .await;
        assert!(res.ok, "create failed: {:?}", res.error);

        let store = state.agents.store.as_ref().unwrap();
        assert_eq!(
            store
                .load_session("mine")
                .await
                .unwrap()
                .unwrap()
                .metadata
                .user_id
                .as_deref(),
            Some("alice")
        );

        // And another person's list does not include it.
        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &RequestContext::new("bob", crate::security::request_context::AuthSource::Session),
        )
        .await;
        let listed = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        assert!(listed.is_empty(), "bob must not see alice's session: {listed:?}");
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
        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &make_test_ctx(),
        )
        .await;
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

    /// The composer's mode picker reads the mode off `sessions.list`, so a
    /// client that just connected shows the mode the session really runs in
    /// rather than assuming the default.
    #[tokio::test]
    async fn sessions_list_reports_the_effective_permission_mode() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        // Materialise the session row (an upsert) so `sessions.list` has it.
        state
            .agents
            .store
            .as_ref()
            .expect("store")
            .set_session_model("s1", None)
            .await
            .expect("create session row");

        // Set the override through the WS method, as the picker does.
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

        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &make_test_ctx(),
        )
        .await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let s1 = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("s1"))
            .expect("s1 should be listed");
        assert_eq!(
            s1.get("permission_mode").and_then(|v| v.as_str()),
            Some("plan"),
            "the override is reported, not the configured default"
        );
    }

    /// A session the store has never seen still has a real mode, and the picker
    /// needs it after a reload: the conversation exists client-side (and in the
    /// override map) long before its first message creates a store row.
    #[tokio::test]
    async fn get_mode_reads_the_override_for_a_session_the_store_lacks() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_get_mode(
            &req("g", "sessions.get_mode", serde_json::json!({ "session_id": "web:dev_abc" })),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok, "get_mode must succeed: {:?}", res.error);
        assert_eq!(res.payload.as_ref().unwrap()["mode"].as_str(), Some("default"));
        assert_eq!(res.payload.as_ref().unwrap()["allow_bypass"], false);

        handle_sessions_set_mode(
            &req(
                "m",
                "sessions.set_mode",
                serde_json::json!({ "session_id": "web:dev_abc", "mode": "plan" }),
            ),
            &conn,
            &state,
        )
        .await;

        let res = handle_sessions_get_mode(
            &req("g", "sessions.get_mode", serde_json::json!({ "session_id": "web:dev_abc" })),
            &conn,
            &state,
        )
        .await;
        assert_eq!(res.payload.as_ref().unwrap()["mode"].as_str(), Some("plan"));

        // Another conversation is unaffected.
        let res = handle_sessions_get_mode(
            &req("g", "sessions.get_mode", serde_json::json!({ "session_id": "web:dev_other" })),
            &conn,
            &state,
        )
        .await;
        assert_eq!(res.payload.as_ref().unwrap()["mode"].as_str(), Some("default"));
    }

    #[tokio::test]
    async fn get_mode_missing_params_errors() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_get_mode(
            &WsRequest {
                frame_type: "req".to_string(),
                id: "g".to_string(),
                method: "sessions.get_mode".to_string(),
                params: None,
            },
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("INVALID_PARAMS"));
    }

    /// A turn must be visible to a client that did not start it — a reload
    /// mid-turn, a reconnect, or a turn started from the CLI or a channel.
    /// `sessions.status` answers from the gateway's own record, and says when
    /// the turn is parked on a human rather than working.
    #[tokio::test]
    async fn status_reports_a_turn_in_flight_and_a_pending_approval() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);

        async fn status(state: &Arc<GatewayState>, session_id: &str) -> serde_json::Value {
            let res = handle_sessions_status(
                &req("s", "sessions.status", serde_json::json!({ "session_id": session_id })),
                &make_test_conn(&["read"]),
                state,
            )
            .await;
            assert!(res.ok, "sessions.status must succeed: {:?}", res.error);
            res.payload.expect("payload")
        }

        let payload = status(&state, "s1").await;
        assert_eq!(payload["running"], false);
        assert_eq!(payload["awaiting_approval"], false);

        state.agents.mark_turn_started("s1").await;
        let payload = status(&state, "s1").await;
        assert_eq!(payload["running"], true);
        assert_eq!(payload["awaiting_approval"], false);

        // A pending approval naming the conversation means the turn is not
        // stalled — it is waiting for an answer only the human can give.
        let pa = crate::tools::approval::PendingApproval::new(
            "a1",
            "shell",
            serde_json::json!({ "command": "ls" }),
            "agent",
        )
        .with_session(Some("s1".to_string()));
        state.tools.approval_queue.submit(pa).await;
        assert_eq!(status(&state, "s1").await["awaiting_approval"], true);
        // Another conversation is neither running nor waiting.
        let other = status(&state, "s2").await;
        assert_eq!(other["running"], false);
        assert_eq!(other["awaiting_approval"], false);

        // Once the turn ends the answer reverts, even with the approval row
        // still present: a stale row must not claim a live turn.
        state.agents.mark_turn_finished("s1").await;
        let payload = status(&state, "s1").await;
        assert_eq!(payload["running"], false);
        assert_eq!(payload["awaiting_approval"], false);
    }

    /// The sidebar reads `running` straight off `sessions.list`; a session
    /// whose turn is in flight must say so there too.
    #[tokio::test]
    async fn list_marks_the_session_whose_turn_is_running() {
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
        state.agents.mark_turn_started("s1").await;

        let res = handle_sessions_list(
            &req("l", "sessions.list", serde_json::json!({})),
            &state,
            &make_test_ctx(),
        )
        .await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let running_of = |sid: &str| {
            sessions
                .iter()
                .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some(sid))
                .and_then(|s| s.get("running"))
                .and_then(|v| v.as_bool())
        };
        assert_eq!(running_of("s1"), Some(true));
        assert_eq!(running_of("s2"), Some(false));
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

    /// Handing a session out and taking it elsewhere: the conversation and the
    /// session state that is not in the messages both travel.
    #[tokio::test]
    async fn handoff_writes_a_bundle_that_another_gateway_can_take() {
        let state = state_with_undoing_agent(vec![]).await;
        let store = state.agents.store.clone().expect("store");
        for (role, content) in [("user", "what broke?"), ("assistant", "the parser")] {
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
        // After the messages, because the first append is what creates the
        // session row this writes to.
        store
            .set_session_name("s1", "release triage")
            .await
            .expect("name");
        let conn = make_test_conn(&["read", "write"]);

        let written = handle_sessions_handoff(
            &req("r1", "sessions.handoff", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;
        assert!(written.ok, "{:?}", written.error);
        let payload = written.payload.expect("payload");
        assert_eq!(payload["messages"], 2);
        let file = payload["file_name"]
            .as_str()
            .expect("file_name")
            .to_string();

        // The bundle carries the name the messages cannot.
        let path = state.infra.transcript_store.root_dir().join(&file);
        let text = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(text.contains("release triage"), "{text}");
        assert!(text.contains("\"version\""), "{text}");

        // Taking it elsewhere: the messages arrive, and the name is applied to
        // a session that has none.
        let taken = handle_sessions_handoff_take(
            &req(
                "r2",
                "sessions.handoff_take",
                serde_json::json!({ "session_id": "target", "file": file }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(taken.ok, "{:?}", taken.error);
        let payload = taken.payload.expect("payload");
        assert_eq!(payload["messages"], 2);
        assert_eq!(payload["from_session"], "s1");
        assert_eq!(payload["applied"][0], "name 'release triage'");
        assert_eq!(
            store.get_session_name("target").await.expect("name"),
            Some("release triage".to_string())
        );
        let messages = store
            .get_messages("target", 10, None)
            .await
            .expect("history");
        assert_eq!(messages.len(), 2);

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// A bundle this build cannot read is refused rather than partly applied,
    /// and a path is refused rather than read.
    #[tokio::test]
    async fn handoff_refuses_a_version_it_does_not_know_and_a_path() {
        let state = state_with_undoing_agent(vec![]).await;
        let conn = make_test_conn(&["write"]);
        let dir = state.infra.transcript_store.root_dir().to_path_buf();
        tokio::fs::create_dir_all(&dir).await.expect("dir");
        tokio::fs::write(
            dir.join("future.json"),
            serde_json::json!({
                "version": 99,
                "exported_at": "2026-10-16T00:00:00Z",
                "origin": { "session_id": "other" },
                "messages": [{ "role": "user", "content": "hi", "timestamp": "2026-10-16T00:00:00Z" }],
            })
            .to_string(),
        )
        .await
        .expect("write");

        let res = handle_sessions_handoff_take(
            &req(
                "r1",
                "sessions.handoff_take",
                serde_json::json!({ "session_id": "s1", "file": "future.json" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        let message = res.error.expect("error").message;
        assert!(message.contains("version 99"), "{message}");

        let res = handle_sessions_handoff_take(
            &req(
                "r2",
                "sessions.handoff_take",
                serde_json::json!({ "session_id": "s1", "file": "/etc/passwd" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert!(res.error.expect("error").message.contains("is a path"));
        let _ = tokio::fs::remove_file(dir.join("future.json")).await;
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
