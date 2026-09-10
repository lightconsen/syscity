//! Inbound message dispatch — workers, queue-mode handling, agent fan-out.
//!
//! Extracted from `gateway/mod.rs`. Provides the unified inbound entry worker,
//! the routed-message worker, and the per-message dispatch logic that fans
//! `RoutedMessage` into the resolved agent's ACP queue.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::{AgentCommand, AgentStatus, BufferedMessage, GatewayEvent, GatewayState};
use crate::agent::session_store::AppendMessageParams;
use crate::observe::record::ChannelObservation;

/// Unified worker that consumes `IncomingMessage`s from `inbound_entry`
/// and drives them through the inbound pipeline.
///
/// The pipeline forwards `RoutedMessage`s to `routed_tx`; the separate
/// `process_routed_messages` worker handles actual agent dispatch.
pub(crate) async fn process_inbound_entries(
    state: Arc<GatewayState>,
    mut rx: mpsc::Receiver<crate::channels::IncomingMessage>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("Inbound entry worker received shutdown signal");
                break;
            }
            Some(incoming) = rx.recv() => {
                match state.pipelines.inbound.process(incoming).await {
                    Some(routed) => {
                        info!("Inbound message routed through pipeline: agent={}", routed.agent_id);
                    }
                    None => {
                        info!("Inbound message absorbed by pipeline (debounced or suppressed)");
                    }
                }
            }
        }
    }

    // Drain any in-flight messages with a bounded timeout.
    let drain_deadline = Duration::from_secs(5);
    while let Ok(Some(incoming)) = timeout(drain_deadline, rx.recv()).await {
        if state.pipelines.inbound.process(incoming).await.is_none() {
            warn!("Drain: inbound pipeline absorbed a message (routing returned None)");
        }
    }
    info!("Inbound entry worker stopped");
}

/// Process routed messages from the inbound pipeline.
///
/// Converts `RoutedMessage` into `AgentCommand::ProcessMessage` and
/// forwards it to the resolved agent, respecting `QueueMode`.
pub(crate) async fn process_routed_messages(
    state: Arc<GatewayState>,
    mut rx: mpsc::Receiver<crate::inbound::RoutedMessage>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("Routed message worker received shutdown signal");
                break;
            }
            Some(routed) = rx.recv() => {
                dispatch_routed_message(&state, routed).await;
            }
        }
    }

    // Drain any in-flight routed messages with a bounded timeout.
    let drain_deadline = Duration::from_secs(5);
    while let Ok(Some(routed)) = timeout(drain_deadline, rx.recv()).await {
        dispatch_routed_message(&state, routed).await;
    }
    info!("Routed message worker stopped");
}

/// Dispatch a single `RoutedMessage` to the resolved agent.
///
/// Lock hierarchy observed in this function:
/// `agents.group_manager` → `agents.message_buffer` → `agents.agents`
/// → `infra.runtime_settings`.
pub(crate) async fn dispatch_routed_message(
    state: &Arc<GatewayState>,
    routed: crate::inbound::RoutedMessage,
) {
    if routed.suppress_delivery {
        debug!("Suppressing delivery for session {}", routed.incoming.conversation_id.0);
        return;
    }

    let session_id = routed.incoming.conversation_id.0.clone();
    let agent_id = routed.agent_id.clone();
    let channel = match &routed.incoming.provenance {
        crate::channels::InputProvenance::ExternalUser { channel, .. } => channel.clone(),
        _ => "unknown".to_string(),
    };
    // Channel-layer observation carried into the turn: not debounced here
    // (this is a direct dispatch), enriched when media was attached during
    // inbound processing, routed to the resolved agent.
    let channel_obs = Some(ChannelObservation {
        debounced: false,
        enriched: routed.media_results.is_some(),
        route: Some(agent_id.clone()),
    });

    // ── Group session membership check ───────────────────────────────
    {
        let user_id = &routed.incoming.user_id.0;
        // Look up the group under a short-lived lock, then release it before
        // acquiring the per-group lock. This avoids holding the group-manager
        // read lock across an await point.
        let group_opt = {
            let groups = state.agents.group_manager.read().await;
            groups.get_group(&session_id)
        };
        if let Some(group) = group_opt {
            let group = group.read().await;
            if !group.is_member(user_id) {
                warn!(
                    "User {} is not a member of group session {}, dropping message",
                    user_id, session_id
                );
                return;
            }
            if let Some(member) = group.get_member(user_id) {
                if !member.role.can_participate() {
                    warn!(
                        "User {} (role: {}) cannot participate in group session {}, dropping \
                         message",
                        user_id, member.role, session_id
                    );
                    return;
                }
            }
        }
    }

    // ── Cache runtime settings used for this dispatch ─────────────────
    let (think_level, queue_mode) = {
        let settings = state.infra.runtime_settings.read().await;
        (
            settings
                .get("think.level")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            settings
                .get("queue.mode")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        )
    };

    match routed.queue_mode {
        crate::inbound::QueueMode::Interrupt => {
            // Clear any buffered messages and pending timer for this session
            {
                let mut buffers = state.agents.message_buffer.write().await;
                buffers.remove(&session_id);
            }
            clear_follow_up_timer(state, &session_id).await;
            send_to_agent(
                state,
                AgentDispatch {
                    agent_id: agent_id.clone(),
                    session_id: session_id.clone(),
                    message: routed.incoming.content.clone(),
                    user_id: routed.incoming.user_id.0.clone(),
                    channel: channel.clone(),
                    think_level: think_level.clone(),
                    queue_mode: queue_mode.clone(),
                    channel_obs: channel_obs.clone(),
                    mentions: routed.incoming.metadata.extra.get("mentions").cloned(),
                },
            )
            .await;
        }

        crate::inbound::QueueMode::Steer => {
            // Send Cancel to agent (best-effort), then send the steer message
            {
                let agents = state.agents.agents.read().await;
                if let Some(agent) = agents.get(&agent_id) {
                    if let Err(e) = agent.tx.send(AgentCommand::Cancel).await {
                        warn!("Failed to send cancel to agent {}: {}", agent_id, e);
                    }
                }
            }
            clear_follow_up_timer(state, &session_id).await;
            // Small delay to let cancel take effect
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            send_to_agent(
                state,
                AgentDispatch {
                    agent_id: agent_id.clone(),
                    session_id: session_id.clone(),
                    message: routed.incoming.content.clone(),
                    user_id: routed.incoming.user_id.0.clone(),
                    channel: channel.clone(),
                    think_level: think_level.clone(),
                    queue_mode: queue_mode.clone(),
                    channel_obs: channel_obs.clone(),
                    mentions: routed.incoming.metadata.extra.get("mentions").cloned(),
                },
            )
            .await;
        }

        crate::inbound::QueueMode::FollowUp => {
            // Buffer message; flush after a delay if no more arrive.
            let should_flush = {
                let mut buffers = state.agents.message_buffer.write().await;
                let buffer = buffers.entry(session_id.clone()).or_default();
                buffer.push(BufferedMessage {
                    content: routed.incoming.content.clone(),
                    user_id: routed.incoming.user_id.0.clone(),
                    channel: channel.clone(),
                });
                buffer.len() >= 5 // Max 5 messages before forced flush
            };

            if should_flush {
                clear_follow_up_timer(state, &session_id).await;
                flush_session_buffer(
                    state,
                    &agent_id,
                    &session_id,
                    think_level.clone(),
                    queue_mode.clone(),
                )
                .await;
            } else {
                // Always register the timer. insert_join aborts any existing
                // timer for this session under the registry lock, eliminating
                // the race between contains() and insert.
                let timer_name = format!("followup:{}", session_id);
                let state_clone = state.clone();
                let agent_id_clone = agent_id.clone();
                let session_id_clone = session_id.clone();
                let think_level_clone = think_level.clone();
                let queue_mode_clone = queue_mode.clone();
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    flush_session_buffer(
                        &state_clone,
                        &agent_id_clone,
                        &session_id_clone,
                        think_level_clone,
                        queue_mode_clone,
                    )
                    .await;
                });
                state.task_registry.insert_join(timer_name, handle).await;
            }
        }

        crate::inbound::QueueMode::Collect => {
            // /done trigger: flush the buffer
            clear_follow_up_timer(state, &session_id).await;
            let has_buffered = {
                let buffers = state.agents.message_buffer.read().await;
                buffers
                    .get(&session_id)
                    .map(|b| !b.is_empty())
                    .unwrap_or(false)
            };

            if has_buffered {
                flush_session_buffer(
                    state,
                    &agent_id,
                    &session_id,
                    think_level.clone(),
                    queue_mode.clone(),
                )
                .await;
            } else {
                // No buffer to flush; treat as normal message
                send_to_agent(
                    state,
                    AgentDispatch {
                        agent_id: agent_id.clone(),
                        session_id: session_id.clone(),
                        message: routed.incoming.content.clone(),
                        user_id: routed.incoming.user_id.0.clone(),
                        channel: channel.clone(),
                        think_level: think_level.clone(),
                        queue_mode: queue_mode.clone(),
                        channel_obs: channel_obs.clone(),
                        mentions: routed.incoming.metadata.extra.get("mentions").cloned(),
                    },
                )
                .await;
            }
        }

        crate::inbound::QueueMode::Normal => {
            send_to_agent(
                state,
                AgentDispatch {
                    agent_id: agent_id.clone(),
                    session_id: session_id.clone(),
                    message: routed.incoming.content.clone(),
                    user_id: routed.incoming.user_id.0.clone(),
                    channel: channel.clone(),
                    think_level: think_level.clone(),
                    queue_mode: queue_mode.clone(),
                    channel_obs: channel_obs.clone(),
                    mentions: routed.incoming.metadata.extra.get("mentions").cloned(),
                },
            )
            .await;
        }
    }
}

/// Cancel any pending FollowUp flush timer for a session.
async fn clear_follow_up_timer(state: &GatewayState, session_id: &str) {
    state
        .task_registry
        .abort(&format!("followup:{}", session_id))
        .await;
}

/// Flush buffered messages for a session and send as a single batch.
pub(crate) async fn flush_session_buffer(
    state: &Arc<GatewayState>,
    agent_id: &str,
    session_id: &str,
    think_level: Option<String>,
    queue_mode: Option<String>,
) {
    let messages: Vec<BufferedMessage> = {
        let mut buffers = state.agents.message_buffer.write().await;
        buffers.remove(session_id).unwrap_or_default()
    };

    if messages.is_empty() {
        return;
    }

    let combined = messages
        .iter()
        .map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n\n");

    let first_user_id = messages
        .first()
        .map(|m| m.user_id.clone())
        .unwrap_or_default();
    let first_channel = messages
        .first()
        .map(|m| m.channel.clone())
        .unwrap_or_default();

    info!(
        "Flushing {} buffered messages for session {} (combined length: {})",
        messages.len(),
        session_id,
        combined.len()
    );

    send_to_agent(
        state,
        AgentDispatch {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            message: combined,
            user_id: first_user_id,
            channel: first_channel,
            think_level,
            queue_mode,
            // This message is the result of a debounce flush of buffered
            // follow-ups; no media enrichment happened at this point.
            channel_obs: Some(ChannelObservation {
                debounced: true,
                enriched: false,
                route: Some(agent_id.to_string()),
            }),
            // A debounce batch coalesces several user turns; per-turn chip
            // attribution is meaningless here, so mentions are dropped.
            mentions: None,
        },
    )
    .await;
}

/// Extract a concise session name from the first assistant response.
/// Strips markdown, takes the first meaningful words, and limits length.
pub(crate) fn extract_session_name(content: &str) -> String {
    // Strip common markdown patterns
    let cleaned = content
        .replace("#", "")
        .replace("**", "")
        .replace("*", "")
        .replace("`", "")
        .replace(">", "")
        .replace("-", "")
        .replace("|", "")
        .replace("\n", " ")
        .replace("\r", " ");

    let name = cleaned
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join(" ");

    if name.len() > 40 {
        format!("{}...", &name[..40])
    } else if name.is_empty() {
        "New Session".to_string()
    } else {
        name
    }
}

/// Payload for dispatching a single message to an agent via ACP.
#[derive(Clone)]
pub(crate) struct AgentDispatch {
    /// Target agent ID.
    pub agent_id: String,
    /// Target session ID.
    pub session_id: String,
    /// Message content.
    pub message: String,
    /// Originating user ID.
    pub user_id: String,
    /// Originating channel.
    pub channel: String,
    /// Optional thinking level from runtime settings.
    pub think_level: Option<String>,
    /// Optional queue mode override.
    pub queue_mode: Option<String>,
    /// Inbound channel-layer observation (debounce/enrich/route) carried into
    /// the turn for observability. `None` for non-dispatch paths.
    pub channel_obs: Option<ChannelObservation>,
    /// Entity chips attached by the web composer (`metadata.extra["mentions"]`
    /// of the routed message), forwarded so the engine can inject the hidden
    /// per-turn mentions block. `None` for paths without chips.
    pub mentions: Option<serde_json::Value>,
}

/// Resolve the effective concrete model ID for a session: the session's
/// explicit pin first, then the bound agent's configured model, else `None`
/// (caller falls back to the global default via the model router's fallback
/// chains).
pub(crate) async fn resolve_session_model(
    state: &Arc<GatewayState>,
    session_id: &str,
    agent_id: &str,
) -> Option<String> {
    if let Some(ref store) = state.agents.store {
        if let Ok(Some(ps)) = store.load_session(session_id).await {
            if let Some(m) = ps.metadata.model.filter(|m| !m.is_empty()) {
                return Some(m);
            }
        }
    }
    state
        .config
        .read()
        .await
        .agent_models
        .get(agent_id)
        .cloned()
}

/// Send a single message to an agent via the ACP controller.
///
/// This routes execution through the centralized ACP actor queue,
/// enabling per-session serial processing and runtime controls
/// (pause / resume / step / cancel).
pub(crate) async fn send_to_agent(state: &Arc<GatewayState>, dispatch: AgentDispatch) {
    let AgentDispatch {
        ref agent_id,
        ref session_id,
        ref message,
        ref user_id,
        ref channel,
        think_level,
        queue_mode,
        channel_obs,
        mentions,
    } = dispatch;

    // UserPromptSubmit gate: a configured shell hook can block a message
    // before it reaches the agent. A blocked message must never trigger an
    // on-demand spawn, broadcast a Processing status, or reach the queue —
    // it is dropped here with a chat.error to the caller.
    if let Some(reason) = state
        .infra
        .shell_hooks
        .check_user_prompt(session_id, user_id, message, channel)
        .await
    {
        if let Err(e) = state.events.tx.send(GatewayEvent::ProcessingError {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            message: reason,
            code: None,
        }) {
            debug!("No receivers for ProcessingError event: {}", e);
        }
        return;
    }

    // Pre-read directive settings so the progress callback does not need to
    // capture the runtime_settings Arc across await boundaries.
    let (reasoning_vis, verbose_mode) = {
        let settings = state.infra.runtime_settings.read().await;
        (
            settings
                .get("reasoning.visibility")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            settings
                .get("verbose.mode")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        )
    };

    let agent_handle = {
        let agents = state.agents.agents.read().await;
        match agents.get(agent_id).cloned() {
            Some(h) => {
                drop(agents);
                h
            }
            None => {
                drop(agents);
                // Agent not yet spawned — try on-demand spawn from personality registry.
                let personality = {
                    let registry = state.agents.registry.read().await;
                    registry.get(agent_id).cloned()
                };
                let Some(personality) = personality else {
                    error!("Agent {} not found in registry for session {}", agent_id, session_id);
                    return;
                };

                let config = personality.to_agent_config();
                info!(
                    "On-demand spawning agent '{}' from personality (dispatch, session={})",
                    agent_id, session_id
                );

                if let Err(e) = crate::gateway::agent_spawn::spawn_agent_inner(
                    state.clone(),
                    agent_id.to_string(),
                    config,
                )
                .await
                {
                    error!("Failed to on-demand spawn agent {}: {}", agent_id, e);
                    return;
                }

                // Wait briefly for the handle to appear (concurrent spawn may have finished).
                let mut handle = None;
                for _ in 0..10 {
                    let agents = state.agents.agents.read().await;
                    handle = agents.get(agent_id).cloned();
                    drop(agents);
                    if handle.is_some() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                match handle {
                    Some(h) => h,
                    None => {
                        error!("Agent {} spawned but handle never appeared", agent_id);
                        return;
                    }
                }
            }
        }
    };

    // Apply thinking config from runtime settings
    let extra = think_level.and_then(|level| {
        let budget = match level.as_str() {
            "minimal" => 1024u32,
            "low" => 4096u32,
            "medium" => 16000u32,
            "high" => 32000u32,
            _ => return None,
        };
        Some(serde_json::json!({ "thinking": { "type": "enabled", "budget_tokens": budget } }))
    });
    agent_handle.agent.set_extra_params(extra).await;

    // Resolve the per-session model binding (session pin -> agent binding ->
    // global default) and apply it scoped to this conversation so concurrent
    // sessions on this shared agent do not interfere.
    let session_model = resolve_session_model(state, session_id, agent_id).await;
    agent_handle
        .agent
        .set_session_model(session_id, session_model)
        .await;

    // Check queue mode and apply interrupt behavior if needed
    if queue_mode.as_deref() == Some("interrupt") {
        if let Err(e) = state.agents.acp.cancel(session_id.to_string()).await {
            warn!("Failed to cancel ACP session {}: {}", session_id, e);
        }
    }

    let mut incoming_msg = crate::channels::IncomingMessage::new(
        user_id.to_string(),
        session_id.to_string(),
        message.to_string(),
    )
    .with_provenance(crate::channels::InputProvenance::ExternalUser {
        channel: channel.to_string(),
        is_direct: true,
    });

    // Carry the channel-layer observation into the turn via the message's
    // extra metadata; the agent drains it onto the turn record.
    if let Some(obs) = channel_obs {
        incoming_msg.metadata.extra.insert(
            "channel_observation".to_string(),
            serde_json::to_value(obs).unwrap_or(serde_json::Value::Null),
        );
    }

    // Re-attach composer chips: this function rebuilds the IncomingMessage
    // from bare dispatch fields, so the routed message's extra metadata would
    // otherwise be lost here (same mechanism as channel_observation above).
    if let Some(m) = mentions {
        incoming_msg
            .metadata
            .extra
            .insert("mentions".to_string(), m);
    }

    // Broadcast processing status
    if let Err(e) = state.events.tx.send(GatewayEvent::AgentStatus {
        agent_id: agent_id.to_string(),
        status: AgentStatus::Processing {
            session_id: session_id.to_string(),
        },
    }) {
        debug!("No receivers for AgentStatus event: {}", e);
    }

    // Build progress callback that forwards events to gateway subscribers
    let event_tx = state.events.tx.clone();
    let progress_session = session_id.to_string();
    let progress_agent = agent_id.to_string();
    let progress_channel = channel.to_string();
    let progress_cb: crate::agent::ProgressCallback = Arc::new(move |event| {
        let tx = event_tx.clone();
        let reasoning_vis = reasoning_vis.clone();
        let verbose_mode = verbose_mode.clone();
        let sid = progress_session.clone();
        let aid = progress_agent.clone();
        let _ch = progress_channel.clone();
        Box::pin(async move {
            match event {
                crate::agent::ProgressEvent::Started => {
                    if let Err(e) = tx.send(GatewayEvent::AgentStatus {
                        agent_id: aid.clone(),
                        status: AgentStatus::Processing { session_id: sid.clone() },
                    }) {
                        debug!("No receivers for AgentStatus event: {}", e);
                    }
                }
                crate::agent::ProgressEvent::Generating { content } => {
                    // Skip reasoning events if visibility is off
                    if reasoning_vis.as_deref() == Some("off") {
                        return;
                    }
                    // Only emit thinking events when there's actual content
                    if let Some(ref thinking) = content {
                        if !thinking.is_empty() {
                            if let Err(e) = tx.send(GatewayEvent::Thinking {
                                session_id: sid.clone(),
                                agent_id: aid.clone(),
                                content: Some(thinking.clone()),
                            }) {
                                debug!("No receivers for Thinking event: {}", e);
                            }
                        }
                    }
                }
                crate::agent::ProgressEvent::ContentDelta { text } => {
                    if let Err(e) = tx.send(GatewayEvent::ContentDelta {
                        session_id: sid.clone(),
                        agent_id: aid.clone(),
                        delta: text,
                    }) {
                        debug!("No receivers for ContentDelta event: {}", e);
                    }
                }
                crate::agent::ProgressEvent::ToolCalling { name, arguments } => {
                    // Skip tool events if verbose is off
                    if verbose_mode.as_deref() == Some("off") {
                        return;
                    }
                    if let Err(e) = tx.send(GatewayEvent::ToolCalling {
                        session_id: sid.clone(),
                        agent_id: aid.clone(),
                        tool_name: name.clone(),
                        arguments: arguments.clone(),
                    }) {
                        debug!("No receivers for ToolCalling event: {}", e);
                    }
                }
                crate::agent::ProgressEvent::ToolResult { name, result, data, .. } => {
                    // Skip tool events if verbose is off
                    if verbose_mode.as_deref() == Some("off") {
                        return;
                    }
                    // In compact verbose mode, truncate long results
                    let result = if verbose_mode.as_deref() == Some("compact") {
                        if result.len() > 500 {
                            format!("{}... (truncated)", &result[..500])
                        } else {
                            result
                        }
                    } else {
                        result
                    };
                    if let Err(e) = tx.send(GatewayEvent::ToolResult {
                        session_id: sid.clone(),
                        agent_id: aid.clone(),
                        tool_name: name.clone(),
                        result,
                        data,
                    }) {
                        debug!("No receivers for ToolResult event: {}", e);
                    }
                }
                crate::agent::ProgressEvent::ToolResultDelta { .. } => {
                    // Streaming tool chunks are accumulated locally and emitted
                    // as a final ToolResult event; no per-chunk gateway event
                    // yet.
                }
                crate::agent::ProgressEvent::Completed { response, turn_id, usage } => {
                    if let Err(e) = tx.send(GatewayEvent::Completed {
                        session_id: sid.clone(),
                        agent_id: aid.clone(),
                        response,
                        turn_id,
                        usage,
                    }) {
                        debug!("No receivers for Completed event: {}", e);
                    }
                }
                crate::agent::ProgressEvent::Error { message } => {
                    if let Err(e) = tx.send(GatewayEvent::ProcessingError {
                        session_id: sid.clone(),
                        agent_id: aid.clone(),
                        message,
                        code: None,
                    }) {
                        debug!("No receivers for ProcessingError event: {}", e);
                    }
                }
            }
        })
    });

    // Route through ACP for serialized execution
    match state
        .agents
        .acp
        .execute_session_with_progress(agent_handle.agent.clone(), incoming_msg, progress_cb)
        .await
    {
        Ok(mut outgoing) => {
            // Apply reasoning visibility filter
            let reasoning_vis = {
                let s = state.infra.runtime_settings.read().await;
                s.get("reasoning.visibility")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            };
            if reasoning_vis.as_deref() == Some("off") {
                outgoing.reasoning_content = None;
            }

            // Accumulate usage statistics (minimal write-lock hold)
            if let Some(ref usage) = outgoing.usage {
                {
                    let mut settings = state.infra.runtime_settings.write().await;
                    let current_tokens = settings
                        .get("usage.tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let total_tokens = usage.prompt_tokens as u64 + usage.completion_tokens as u64;
                    settings.insert(
                        "usage.tokens".to_string(),
                        serde_json::json!(current_tokens + total_tokens),
                    );
                    let current_calls = settings
                        .get("usage.calls")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let tool_calls = outgoing
                        .tool_calls
                        .as_ref()
                        .map(|c| c.len() as u64)
                        .unwrap_or(0);
                    settings.insert(
                        "usage.calls".to_string(),
                        serde_json::json!(current_calls + tool_calls + 1),
                    );
                }
            }

            // Generate run_id for this agent execution (run tracking)
            let run_id = uuid::Uuid::new_v4().to_string();

            // Save assistant response to persistent session history
            if let Some(ref store) = state.agents.store {
                let reasoning = outgoing.reasoning_content.as_deref();
                let tool_calls_json = outgoing
                    .tool_calls
                    .as_ref()
                    .map(|calls| serde_json::to_string(calls).unwrap_or_default());
                if let Err(e) = store
                    .append_message(&AppendMessageParams {
                        session_id,
                        role: "assistant",
                        content: &outgoing.content,
                        reasoning_content: reasoning,
                        tool_calls_json: tool_calls_json.as_deref(),
                        transcript_id: Some(session_id),
                        run_id: Some(&run_id),
                        ..Default::default()
                    })
                    .await
                {
                    warn!("Failed to save assistant message to session history: {}", e);
                }

                // Auto-name session from first assistant response if no name yet
                if let Ok(existing) = store.get_session_name(session_id).await {
                    if existing.is_none() {
                        let name = extract_session_name(&outgoing.content);
                        if let Err(e) = store.set_session_name(session_id, &name).await {
                            warn!("Failed to save session name: {}", e);
                        } else {
                            info!("Session {} auto-named: '{}'", session_id, name);
                            if let Err(e) = state.events.tx.send(GatewayEvent::SessionRenamed {
                                session_id: session_id.to_string(),
                                name: name.clone(),
                            }) {
                                debug!("No receivers for SessionRenamed event: {}", e);
                            }
                        }
                    }
                }
            }
            if let Err(e) = state.events.tx.send(GatewayEvent::AgentResponse {
                session_id: session_id.to_string(),
                agent_id: agent_id.to_string(),
                content: outgoing.content,
                channel: channel.to_string(),
                conversation_id: session_id.to_string(),
                usage: outgoing.usage,
            }) {
                debug!("No receivers for AgentResponse event: {}", e);
            }
        }
        Err(e) => {
            error!("ACP execution failed for agent {} session {}: {}", agent_id, session_id, e);
            if let Err(e) = state.events.tx.send(GatewayEvent::ProcessingError {
                session_id: session_id.to_string(),
                agent_id: agent_id.to_string(),
                message: format!("Execution failed: {}", e),
                code: extract_error_code(&e),
            }) {
                debug!("No receivers for ProcessingError event: {}", e);
            }
        }
    }

    if let Err(e) = state.events.tx.send(GatewayEvent::AgentStatus {
        agent_id: agent_id.to_string(),
        status: AgentStatus::Idle,
    }) {
        debug!("No receivers for AgentStatus event: {}", e);
    }

    // Stop hook: fire-and-forget shell commands for the turn that just ended.
    state
        .infra
        .shell_hooks
        .fire_stop(session_id, agent_id, channel);
}

/// Extract a machine-readable error code from a provider failure when one
/// is recognizable. The cloud relay returns HTTP 403 with an
/// `insufficient_credits` body when the credit balance falls below the
/// overdraft floor; surfacing the code lets clients render a recharge
/// prompt instead of a generic error. Always `None` without the cloud
/// feature.
#[cfg(feature = "cloud")]
fn extract_error_code(e: &crate::error::SyscityError) -> Option<String> {
    match e {
        crate::error::SyscityError::ExternalService { source, .. } => {
            if source.contains("insufficient_credits") {
                Some("insufficient_credits".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(not(feature = "cloud"))]
fn extract_error_code(_e: &crate::error::SyscityError) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session_store::SessionMetadata;
    use crate::gateway::state_tests::{make_test_state, make_test_state_with_store};
    use crate::gateway::GatewayConfig;

    /// Persist a session row with the given pinned model (if any) for `session_id`.
    async fn seed_session(state: &GatewayState, session_id: &str, model: Option<&str>) {
        let store = state.agents.store.as_ref().expect("store wired in");
        let mut meta = SessionMetadata::new(session_id, "main", "web", "u1");
        meta.model = model.map(String::from);
        store
            .save_session(session_id, &meta, "{}")
            .await
            .expect("save session");
    }

    #[tokio::test]
    async fn resolve_uses_session_pin_over_agent_binding() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        seed_session(&state, "s1", Some("alt")).await;
        {
            let mut config = state.config.write().await;
            let cfg = Arc::make_mut(&mut config);
            cfg.agent_models.insert("main".into(), "agent-model".into());
        }
        let resolved = resolve_session_model(&state, "s1", "main").await;
        assert_eq!(resolved.as_deref(), Some("alt"));
    }

    #[tokio::test]
    async fn resolve_falls_back_to_agent_binding_when_no_pin() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        seed_session(&state, "s1", None).await;
        {
            let mut config = state.config.write().await;
            let cfg = Arc::make_mut(&mut config);
            cfg.agent_models.insert("main".into(), "agent-model".into());
        }
        let resolved = resolve_session_model(&state, "s1", "main").await;
        assert_eq!(resolved.as_deref(), Some("agent-model"));
    }

    #[tokio::test]
    async fn resolve_ignores_empty_pin_and_falls_back() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        seed_session(&state, "s1", Some("")).await;
        {
            let mut config = state.config.write().await;
            let cfg = Arc::make_mut(&mut config);
            cfg.agent_models.insert("main".into(), "agent-model".into());
        }
        let resolved = resolve_session_model(&state, "s1", "main").await;
        assert_eq!(resolved.as_deref(), Some("agent-model"));
    }

    #[tokio::test]
    async fn resolve_returns_none_when_neither_bound() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        seed_session(&state, "s1", None).await;
        let resolved = resolve_session_model(&state, "s1", "main").await;
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn resolve_without_store_uses_agent_binding() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        {
            let mut config = state.config.write().await;
            let cfg = Arc::make_mut(&mut config);
            cfg.agent_models.insert("main".into(), "agent-model".into());
        }
        let resolved = resolve_session_model(&state, "s1", "main").await;
        assert_eq!(resolved.as_deref(), Some("agent-model"));
    }

    fn routed_msg(
        agent_id: &str,
        session: &str,
        queue_mode: crate::inbound::QueueMode,
        suppress_delivery: bool,
    ) -> crate::inbound::RoutedMessage {
        let incoming = crate::channels::IncomingMessage::new(
            "u1".to_string(),
            session.to_string(),
            "hello".to_string(),
        )
        .with_provenance(crate::channels::InputProvenance::ExternalUser {
            channel: "web".to_string(),
            is_direct: true,
        });
        crate::inbound::RoutedMessage {
            incoming,
            agent_id: agent_id.to_string(),
            workspace_id: None,
            queue_mode,
            suppress_delivery,
            media_results: None,
            envelope_context: None,
        }
    }

    #[test]
    fn extract_session_name_strips_markdown() {
        assert_eq!(extract_session_name("# **Hello** `world`"), "Hello world");
    }

    #[test]
    fn extract_session_name_keeps_six_words() {
        assert_eq!(
            extract_session_name("one two three four five six seven"),
            "one two three four five six"
        );
    }

    #[test]
    fn extract_session_name_empty_returns_new_session() {
        assert_eq!(extract_session_name(""), "New Session");
    }

    #[test]
    fn extract_session_name_truncates_long() {
        let long = "a".repeat(60);
        let name = extract_session_name(&long);
        assert!(name.ends_with("..."));
        assert!(name.len() <= 43);
    }

    #[tokio::test]
    async fn dispatch_suppressed_delivery_returns_early() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        dispatch_routed_message(
            &state,
            routed_msg("main", "s1", crate::inbound::QueueMode::Normal, true),
        )
        .await;
        let buffers = state.agents.message_buffer.read().await;
        assert!(buffers.is_empty());
    }

    #[tokio::test]
    async fn dispatch_normal_unknown_agent_noop() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        // Agent is neither spawned nor in the personality registry: the
        // dispatch logs an error and returns without spawning.
        dispatch_routed_message(
            &state,
            routed_msg("ghost", "s1", crate::inbound::QueueMode::Normal, false),
        )
        .await;
        let agents = state.agents.agents.read().await;
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn dispatch_interrupt_unknown_agent_noop() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        dispatch_routed_message(
            &state,
            routed_msg("ghost", "s1", crate::inbound::QueueMode::Interrupt, false),
        )
        .await;
        let buffers = state.agents.message_buffer.read().await;
        assert!(buffers.is_empty());
    }

    #[tokio::test]
    async fn dispatch_collect_empty_buffer_sends() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        dispatch_routed_message(
            &state,
            routed_msg("ghost", "s1", crate::inbound::QueueMode::Collect, false),
        )
        .await;
    }

    #[tokio::test]
    async fn dispatch_follow_up_flushes_after_five_messages() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        for _ in 0..5 {
            dispatch_routed_message(
                &state,
                routed_msg("ghost", "s1", crate::inbound::QueueMode::FollowUp, false),
            )
            .await;
        }
        // The fifth message forces a flush; the buffer must be drained.
        let buffers = state.agents.message_buffer.read().await;
        assert!(buffers.get("s1").map(|b| b.is_empty()).unwrap_or(true));
    }

    #[tokio::test]
    async fn flush_session_buffer_empty_is_noop() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        flush_session_buffer(&state, "main", "s1", None, None).await;
        let buffers = state.agents.message_buffer.read().await;
        assert!(buffers.is_empty());
    }
}
