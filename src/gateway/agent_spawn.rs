//! Agent-spawning logic and tool-registry factory.
//!
//! Extracted from `gateway/mod.rs` to reduce the main control-plane file.
//! Re-exported via `pub use agent_spawn::*;` so callers continue to see
//! `spawn_agent_inner` / `create_default_tool_registry` at the `gateway` level.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::acp::AcpControlPlane;
use crate::agent::session_store::AppendMessageParams;
use crate::agent::{Agent, AgentConfig};
use crate::config::CapabilitiesConfig;
use crate::mcp::McpManager;
use crate::tools::approval::ApprovalQueue;
use crate::tools::ask_user::AskQueue;
use crate::tools::delegate_tool::AgentResolver;
use crate::tools::ToolRegistry;

// ── GatewayAgentResolver ─────────────────────────────────────────────────────

/// Wraps the Gateway agent map for [`AgentResolver`] lookups.
pub(crate) struct GatewayAgentResolver {
    pub(crate) agents: Arc<RwLock<std::collections::HashMap<String, super::AgentHandle>>>,
}

#[async_trait]
impl AgentResolver for GatewayAgentResolver {
    async fn resolve(&self, name: &str) -> Option<Arc<Agent>> {
        let agent = {
            let agents = self.agents.read().await;
            agents.get(name).map(|h| h.agent.clone())
        };
        agent
    }
}

/// Resolves the parent agent for a wake notification (parent auto-wake, v2).
///
/// A root parent lives on a user session (router-bound, resolved via
/// `resolve_by_session`); a delegated parent lives on a `delegation:<run_id>`
/// session (not router-bound, because delegated turns run
/// `process_message_with_progress` directly) — its agent is the `agent_id`
/// recorded on its delegation task row.
pub(crate) struct GatewayWakeResolver {
    pub(crate) agents: Arc<RwLock<std::collections::HashMap<String, super::AgentHandle>>>,
    pub(crate) router: Arc<crate::inbound::router::AgentRouter>,
    pub(crate) store: Arc<crate::delegation::DelegationTaskStore>,
}

#[async_trait]
impl crate::delegation::WakeResolver for GatewayWakeResolver {
    async fn resolve_agent(&self, session: &str) -> Option<Arc<Agent>> {
        let agent_id = if let Some(run_id) = session.strip_prefix("delegation:") {
            self.store
                .get_task(run_id)
                .await
                .ok()
                .flatten()
                .map(|task| task.agent_id)
        } else {
            Some(self.router.resolve_by_session(session).await.agent_id)
        };
        let agent_id = agent_id?;
        let agents = self.agents.read().await;
        agents
            .get(&agent_id)
            .map(|h| h.agent.clone())
            .or_else(|| agents.get("default").map(|h| h.agent.clone()))
    }
}

// ── spawn_agent_inner ────────────────────────────────────────────────────────

/// Resolve the model an agent should use at spawn time.
///
/// Fast mode stores the active model in `runtime_settings` so that toggling it
/// does not require mutating the immutable config snapshot. If fast mode is not
/// active, fall back to the configured default model.
async fn effective_model_for_spawn(state: &super::GatewayState) -> String {
    let settings = state.infra.runtime_settings.read().await;
    if let Some(model) = settings.get("fast.active_model").and_then(|v| v.as_str()) {
        return model.to_string();
    }
    state.config.read().await.model.clone()
}

/// Spawn a single agent, wire it into the Gateway's agent pool, register its
/// computer adapter, and start the per-agent message processing loop. The
/// loop's JoinHandle is owned by the task registry.
pub(crate) async fn spawn_agent_inner(
    state: Arc<super::GatewayState>,
    id: String,
    mut config: AgentConfig,
) -> crate::Result<()> {
    config.agent_id = Some(id.clone());
    info!("Spawning agent: {}", id);

    // Capture the base config before merging per-agent overrides so a later
    // `config.set agent_overrides.*` can recompute the effective config from
    // the same base and push it to this running agent.
    let base_config = config.clone();
    state
        .config
        .read()
        .await
        .apply_agent_overrides(&id, &mut config);

    // Snapshot the 在线质量监控 (§八) config once; agents read it lock-free at
    // the post-turn risk hook (`scan_turn_for_badcase`).
    let online_monitoring = state.config.read().await.eval.online_monitoring.clone();
    // Snapshot the 压缩质量门禁 (§三) config; when enabled, the collector flags
    // low-retention compressions as online risk signals.
    let compression_quality = state.config.read().await.eval.compression_quality.clone();
    // Snapshot the 生产流量在线采样 (§…) config; when enabled, the agent
    // persists a sampled subset of completed turns to the sample store.
    let sampling = state.config.read().await.eval.sampling.clone();

    // Reserve the agent ID across the async setup to prevent concurrent
    // callers from both passing the duplicate check and creating ghost tasks.
    {
        let mut pending = state
            .agents
            .pending_spawns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !pending.insert(id.clone()) {
            return Err(crate::SyscityError::Validation(format!(
                "Agent '{}' spawn already in progress",
                id
            )));
        }
    }

    // Ensure the pending entry is cleared on every exit path.
    struct PendingGuard {
        pending: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        id: String,
    }
    impl Drop for PendingGuard {
        fn drop(&mut self) {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&self.id);
            }
        }
    }
    let _pending_guard = PendingGuard {
        pending: state.agents.pending_spawns.clone(),
        id: id.clone(),
    };

    let (tx, rx) = tokio::sync::mpsc::channel(100);

    // Create provider from model router
    let provider: Arc<dyn crate::providers::Provider> =
        state.infra.model_router.create_default_provider().await?;
    // Get tool registry from state
    let tools = state.tools.registry.clone();

    // Get the model for this agent, honoring any active fast-mode override
    // stored in runtime_settings.
    let model = effective_model_for_spawn(&state).await;

    // Create the actual Agent instance with model, memory manager, chat history,
    // shared cost guard, and session management stores.
    let memory_manager = state.memory.manager.read().await.as_ref().cloned();
    let cost_guard = Arc::clone(&state.agents.cost_guard);

    // Read computer config for the agent
    let computer_config = {
        let cfg = state.config.read().await;
        crate::computer::LoopConfig {
            max_steps: cfg.computer.max_steps,
            settle_delay_ms: cfg.computer.settle_delay_ms,
            ..Default::default()
        }
    };
    let computer_adapter = state.tools.computer_adapter.read().await.clone();

    let agent = if let Some(mm) = memory_manager {
        let chat_history = mm.chat_history();
        let mut builder = Agent::new(config.clone(), provider, tools)
            .with_paths(state.paths.clone())
            .with_model(model.clone())
            .with_memory_manager(mm.clone())
            .with_chat_history(chat_history)
            .with_cost_guard(cost_guard)
            .with_online_monitoring(online_monitoring.clone())
            .with_compression_quality(compression_quality.clone())
            .with_sample_store(state.infra.sample_store.clone())
            .with_sampling(sampling.clone())
            .with_transcript_store(Arc::clone(&state.infra.transcript_store))
            .with_artifact_store(Arc::clone(&state.infra.artifact_store))
            .with_disk_budget(Arc::clone(&state.infra.disk_budget))
            .with_session_file_manager(Arc::clone(&state.infra.session_file_manager))
            .with_model_router(Arc::clone(&state.infra.model_router))
            .with_skill_manager(Arc::clone(&state.tools.skills_manager));
        if let Some(adapter) = computer_adapter.clone() {
            builder = builder
                .with_computer_adapter(adapter)
                .with_computer_config(computer_config);
        }
        // Attach planner state store for crash recovery on restart.
        let planner_db = state.paths.root().join("planner.db");
        let url = format!("sqlite:///{}", planner_db.display());
        if let Ok(store) = crate::planner::TaskStateStore::new(&url).await {
            builder = builder.with_planner_state_store(store);
        }
        if let Some(pending) = state.infra.pending_badcase_store.clone() {
            builder =
                builder.with_badcase_pipeline(crate::eval::RiskSignalChecker::default(), pending);
        }
        Arc::new(builder)
    } else {
        let mut builder = Agent::new(config.clone(), provider, tools)
            .with_paths(state.paths.clone())
            .with_model(model.clone())
            .with_cost_guard(cost_guard)
            .with_online_monitoring(online_monitoring.clone())
            .with_compression_quality(compression_quality.clone())
            .with_sample_store(state.infra.sample_store.clone())
            .with_sampling(sampling.clone())
            .with_skill_manager(Arc::clone(&state.tools.skills_manager))
            .with_transcript_store(Arc::clone(&state.infra.transcript_store))
            .with_artifact_store(Arc::clone(&state.infra.artifact_store))
            .with_disk_budget(Arc::clone(&state.infra.disk_budget))
            .with_session_file_manager(Arc::clone(&state.infra.session_file_manager))
            .with_model_router(Arc::clone(&state.infra.model_router));
        if let Some(adapter) = computer_adapter.clone() {
            builder = builder
                .with_computer_adapter(adapter)
                .with_computer_config(computer_config);
        }
        // Attach planner state store for crash recovery on restart.
        let planner_db = state.paths.root().join("planner.db");
        let url = format!("sqlite:///{}", planner_db.display());
        if let Ok(store) = crate::planner::TaskStateStore::new(&url).await {
            builder = builder.with_planner_state_store(store);
        }
        if let Some(pending) = state.infra.pending_badcase_store.clone() {
            builder =
                builder.with_badcase_pipeline(crate::eval::RiskSignalChecker::default(), pending);
        }
        Arc::new(builder)
    };

    // Set the planner in the shared PlannerTool handle so the LLM can
    // invoke the planner tool.
    if let Some(planner) = &agent.goal_planner {
        if let Ok(mut guard) = state.tools.planner_handle.write() {
            *guard = Some(Arc::clone(planner));
        }
    }

    // Wire the new agent into the cron scheduler so routine (agent-target)
    // jobs can run. Only the first agent is wired; subsequent agents keep
    // the first one active unless explicitly overwritten.
    {
        if let Some(cron_arc) = state.scheduler.cron_scheduler.read().await.clone() {
            cron_arc.lock().await.set_agent(agent.clone()).await;
            debug!("Routine engine: wired agent '{}' into cron scheduler", id);
        }
    }

    let (query_tx, query_rx) = tokio::sync::mpsc::channel::<super::AgentQuery>(32);

    let handle = super::AgentHandle {
        id: id.clone(),
        config: config.clone(),
        base_config,
        tx: tx.clone(),
        query_tx: query_tx.clone(),
        busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        agent: agent.clone(),
    };
    let handle_for_loop = handle.clone();

    {
        let mut agents = state.agents.agents.write().await;
        if agents.contains_key(&id) {
            return Err(crate::SyscityError::Validation(format!(
                "Agent '{}' is already running",
                id
            )));
        }
        agents.insert(id.clone(), handle);
    }

    // Register the per-agent stale-context eviction loop (check every 5 min,
    // evict contexts idle > 30 min) in the task registry so shutdown and agent
    // replacement can abort it uniformly.
    let task_registry = state.task_registry.clone();
    let repair_handle = agent.start_self_repair_loop(
        std::time::Duration::from_secs(300),
        std::time::Duration::from_secs(1800),
    );
    task_registry
        .insert_join(format!("agent:{}:repair", id), repair_handle)
        .await;

    // Start agent processing loop
    let agent_id = id.clone();
    let agent_for_loop = agent.clone();
    let busy = handle_for_loop.busy.clone();
    let state_for_loop = state.clone();

    let task_handle = tokio::spawn(async move {
        run_agent_loop(state_for_loop, agent_id, agent_for_loop, busy, rx, query_rx, true).await;
    });

    task_registry
        .insert_join(format!("agent:{}", id), task_handle)
        .await;

    Ok(())
}

// ── Shared agent processing loop ─────────────────────────────────────────────

/// Shared per-agent message/query processing loop.
///
/// `use_acp` selects between ACP-routed execution (production agents spawned by
/// the gateway) and direct agent execution (agents created via the REST API).
/// Broadcast a gateway event and log if there are no active listeners.
fn emit_event(
    tx: &tokio::sync::broadcast::Sender<super::GatewayEvent>,
    event: super::GatewayEvent,
) {
    if let Err(e) = tx.send(event) {
        warn!("Failed to broadcast gateway event: {}", e);
    }
}

pub(crate) async fn run_agent_loop(
    state: Arc<super::GatewayState>,
    agent_id: String,
    agent: Arc<Agent>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    mut rx: tokio::sync::mpsc::Receiver<super::AgentCommand>,
    mut query_rx: tokio::sync::mpsc::Receiver<super::AgentQuery>,
    use_acp: bool,
) {
    use std::sync::atomic::Ordering;

    info!("Agent {} processing loop started", agent_id);

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let cmd = match cmd { Some(c) => c, None => break };
                match cmd {
                    super::AgentCommand::ProcessMessage {
                        session_id,
                        message,
                        user_id,
                        channel,
                        model_override,
                    } => {
                        busy.store(true, Ordering::Release);
                        emit_event(&state.events.tx, super::GatewayEvent::AgentStatus {
                            agent_id: agent_id.clone(),
                            status: super::AgentStatus::Processing {
                                session_id: session_id.clone(),
                            },
                        });

                        if use_acp {
                            process_message_acp(
                                &state,
                                &agent_id,
                                &agent,
                                &session_id,
                                &message,
                                &user_id,
                                &channel,
                            )
                            .await;
                        } else {
                            process_message_direct(
                                &state,
                                &agent_id,
                                &agent,
                                DirectMessage {
                                    session_id: session_id.clone(),
                                    message: message.clone(),
                                    user_id: user_id.clone(),
                                    channel: channel.clone(),
                                    model_override,
                                },
                            )
                            .await;
                        }

                        emit_event(&state.events.tx, super::GatewayEvent::AgentStatus {
                            agent_id: agent_id.clone(),
                            status: super::AgentStatus::Idle,
                        });
                        busy.store(false, Ordering::Release);
                    }
                    super::AgentCommand::Cancel => {
                        if use_acp {
                            warn!("Agent {} received cancel command (ACP path)", agent_id);
                        } else {
                            warn!("Agent {} received cancel command", agent_id);
                        }
                    }
                    super::AgentCommand::UpdateConfig(new_config) => {
                        info!("Agent {} updating configuration", agent_id);
                        {
                            let mut agents = state.agents.agents.write().await;
                            if let Some(handle) = agents.get_mut(&agent_id) {
                                handle.config = new_config.clone();
                                // Apply to the actual runtime too, not just the
                                // display copy, so hot-reload / config.set takes
                                // effect from the next turn onward.
                                handle.agent.update_config(new_config);
                                info!("Agent {} configuration updated", agent_id);
                            }
                        }
                        emit_event(&state.events.tx, super::GatewayEvent::AgentStatus {
                            agent_id: agent_id.clone(),
                            status: super::AgentStatus::Idle,
                        });
                    }
                    super::AgentCommand::Shutdown => {
                        info!("Agent {} shutting down", agent_id);
                        emit_event(&state.events.tx, super::GatewayEvent::AgentStatus {
                            agent_id: agent_id.clone(),
                            status: super::AgentStatus::Shutdown,
                        });
                        break;
                    }
                }
            }
            query = query_rx.recv() => {
                let query = match query { Some(q) => q, None => break };
                match query {
                    super::AgentQuery::GetThreadSummaries { response_tx } => {
                        if let Err(e) = response_tx.send(agent.thread_summaries().await) {
                            warn!("Agent {}: failed to send thread summaries: {:?}", agent_id, e);
                        }
                    }
                    super::AgentQuery::GetThreadTurns { conv_id, response_tx } => {
                        if let Err(e) = response_tx.send(agent.thread_turns_for(&conv_id).await) {
                            warn!(
                                "Agent {}: failed to send thread turns for {}: {:?}",
                                agent_id, conv_id, e
                            );
                        }
                    }
                    super::AgentQuery::UndoLastTurn { conv_id, response_tx } => {
                        if let Err(e) = response_tx.send(agent.undo_last_turn(&conv_id).await) {
                            warn!(
                                "Agent {}: failed to send undo result for {}: {:?}",
                                agent_id, conv_id, e
                            );
                        }
                    }
                    super::AgentQuery::RedoLastTurn { conv_id, response_tx } => {
                        if let Err(e) = response_tx.send(agent.redo_last_turn(&conv_id).await) {
                            warn!(
                                "Agent {}: failed to send redo result for {}: {:?}",
                                agent_id, conv_id, e
                            );
                        }
                    }
                    super::AgentQuery::RunSkill {
                        session_id,
                        message,
                        user_id,
                        skill_trust,
                        response_tx,
                    } => {
                        agent.set_skill_trust(skill_trust);
                        let incoming = crate::channels::IncomingMessage::new(
                            user_id,
                            &session_id,
                            message,
                        );
                        let no_op: crate::agent::ProgressCallback =
                            Arc::new(|_| Box::pin(async {}));
                        let result =
                            agent.process_message_with_progress(incoming, no_op).await;
                        agent.set_skill_trust(crate::tools::SkillTrust::Trusted);
                        if let Err(e) = response_tx.send(result) {
                            warn!(
                                "Agent {}: failed to send skill run result for {}: {:?}",
                                agent_id, session_id, e
                            );
                        }
                    }
                }
            }
        }
    }

    info!("Agent {} processing loop ended", agent_id);
}

/// Process a message through the ACP control plane.
async fn process_message_acp(
    state: &Arc<super::GatewayState>,
    agent_id: &str,
    agent: &Arc<Agent>,
    session_id: &str,
    message: &str,
    user_id: &str,
    channel: &str,
) {
    let incoming_msg = crate::channels::IncomingMessage::new(
        user_id.to_string(),
        session_id.to_string(),
        message.to_string(),
    )
    .with_provenance(crate::channels::InputProvenance::ExternalUser {
        channel: channel.to_string(),
        is_direct: true,
    });

    let progress_state = state.clone();
    let progress_session_id = session_id.to_string();
    let progress_agent_id = agent_id.to_string();
    let progress_cb: crate::agent::ProgressCallback = Arc::new(move |event| {
        let state = progress_state.clone();
        let session_id = progress_session_id.clone();
        let agent_id = progress_agent_id.clone();
        Box::pin(async move {
            match event {
                crate::agent::ProgressEvent::ToolCalling { name, arguments } => {
                    info!("ToolCalling event: {} for session {}", name, session_id);
                    emit_event(
                        &state.events.tx,
                        super::GatewayEvent::ToolCalling {
                            session_id: session_id.clone(),
                            agent_id: agent_id.clone(),
                            tool_name: name.clone(),
                            arguments: arguments.clone(),
                        },
                    );
                }
                crate::agent::ProgressEvent::ToolResult { name, result, data, .. } => {
                    info!("ToolResult event: {} for session {}", name, session_id);
                    emit_event(
                        &state.events.tx,
                        super::GatewayEvent::ToolResult {
                            session_id: session_id.clone(),
                            agent_id: agent_id.clone(),
                            tool_name: name.clone(),
                            result: result.clone(),
                            data: data.clone(),
                        },
                    );
                }
                crate::agent::ProgressEvent::Completed { response, turn_id, usage } => {
                    emit_event(
                        &state.events.tx,
                        super::GatewayEvent::Completed {
                            session_id: session_id.clone(),
                            agent_id: agent_id.clone(),
                            response,
                            turn_id,
                            usage,
                        },
                    );
                }
                crate::agent::ProgressEvent::Error { message } => {
                    emit_event(
                        &state.events.tx,
                        super::GatewayEvent::ProcessingError {
                            session_id: session_id.clone(),
                            agent_id: agent_id.clone(),
                            message,
                            code: None,
                        },
                    );
                }
                _ => {}
            }
        })
    });

    let (response_content, response_usage, response_reasoning) = match agent
        .process_message_with_progress(incoming_msg, progress_cb)
        .await
    {
        Ok(outgoing) => {
            let reasoning = outgoing.reasoning_content.clone();
            (outgoing.content, outgoing.usage, reasoning)
        }
        Err(e) => {
            error!("Agent {} failed to process message: {}", agent_id, e);
            (format!("Error processing message: {}", e), None, None)
        }
    };

    let conversation_id = {
        let sessions = state.channels.session_channels.read().await;
        sessions
            .get(session_id)
            .map(|(_, cid)| cid.clone())
            .unwrap_or_else(|| session_id.to_string())
    };

    let run_id = uuid::Uuid::new_v4().to_string();

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store
            .append_message(&AppendMessageParams {
                session_id,
                role: "assistant",
                content: &response_content,
                transcript_id: Some(session_id),
                run_id: Some(&run_id),
                ..Default::default()
            })
            .await
        {
            warn!("Failed to save assistant message to session history: {}", e);
        }
    }

    info!(
        "DEBUG: Agent {} sending AgentResponse for session {} (conversation: {})",
        agent_id, session_id, conversation_id
    );
    emit_event(
        &state.events.tx,
        super::GatewayEvent::AgentResponse {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            content: response_content.clone(),
            channel: channel.to_string(),
            conversation_id: conversation_id.clone(),
            usage: response_usage,
        },
    );

    let outbound_ctx = {
        let cfg = state.config.read().await;
        crate::outbound::OutboundContext {
            session_id: session_id.to_string(),
            channel: channel.to_string(),
            agent_id: agent_id.to_string(),
            raw_output: response_content,
            tool_calls: vec![],
            usage: response_usage,
            side_effects: vec![],
            model_name: Some(cfg.model.clone()),
            model_provider: Some(cfg.model_provider.clone()),
            reasoning_content: response_reasoning,
        }
    };
    let outbound_result = state.pipelines.outbound.process(outbound_ctx).await;

    if let Some(canvas_update) = outbound_result.canvas_update {
        state
            .tools
            .canvas_manager
            .apply_update(session_id, canvas_update)
            .await;
    }
}

/// Message payload for direct (non-ACP) agent execution.
struct DirectMessage {
    /// Target session ID.
    session_id: String,
    /// Message content.
    message: String,
    /// Originating user ID.
    user_id: String,
    /// Originating channel.
    channel: String,
    /// Optional model override.
    model_override: Option<String>,
}

/// Process a message directly (no ACP serialization).
async fn process_message_direct(
    state: &Arc<super::GatewayState>,
    agent_id: &str,
    agent: &Arc<Agent>,
    msg: DirectMessage,
) {
    use crate::agent::session_store::AppendMessageParams;

    let DirectMessage {
        ref session_id,
        ref message,
        ref user_id,
        ref channel,
        model_override,
    } = msg;

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

    let event_tx = state.events.tx.clone();
    let progress_session = session_id.to_string();
    let progress_agent = agent_id.to_string();
    let progress_cb: crate::agent::ProgressCallback = Arc::new(move |event| {
        let tx = event_tx.clone();
        let reasoning_vis = reasoning_vis.clone();
        let verbose_mode = verbose_mode.clone();
        let sid = progress_session.clone();
        let aid = progress_agent.clone();
        Box::pin(async move {
            match event {
                crate::agent::ProgressEvent::Started => {
                    emit_event(
                        &tx,
                        super::GatewayEvent::AgentStatus {
                            agent_id: aid.clone(),
                            status: super::AgentStatus::Processing { session_id: sid.clone() },
                        },
                    );
                }
                crate::agent::ProgressEvent::Generating { content } => {
                    if reasoning_vis.as_deref() == Some("off") {
                        return;
                    }
                    if let Some(ref thinking) = content {
                        if !thinking.is_empty() {
                            emit_event(
                                &tx,
                                super::GatewayEvent::Thinking {
                                    session_id: sid.clone(),
                                    agent_id: aid.clone(),
                                    content: Some(thinking.clone()),
                                },
                            );
                        }
                    }
                }
                crate::agent::ProgressEvent::ContentDelta { text } => {
                    emit_event(
                        &tx,
                        super::GatewayEvent::ContentDelta {
                            session_id: sid.clone(),
                            agent_id: aid.clone(),
                            delta: text,
                        },
                    );
                }
                crate::agent::ProgressEvent::ToolCalling { name, arguments } => {
                    if verbose_mode.as_deref() == Some("off") {
                        return;
                    }
                    emit_event(
                        &tx,
                        super::GatewayEvent::ToolCalling {
                            session_id: sid.clone(),
                            agent_id: aid.clone(),
                            tool_name: name.clone(),
                            arguments: arguments.clone(),
                        },
                    );
                }
                crate::agent::ProgressEvent::ToolResult { name, result, data, .. } => {
                    if verbose_mode.as_deref() == Some("off") {
                        return;
                    }
                    let result = if verbose_mode.as_deref() == Some("compact") {
                        if result.len() > 500 {
                            format!("{}... (truncated)", &result[..500])
                        } else {
                            result
                        }
                    } else {
                        result
                    };
                    emit_event(
                        &tx,
                        super::GatewayEvent::ToolResult {
                            session_id: sid.clone(),
                            agent_id: aid.clone(),
                            tool_name: name.clone(),
                            result,
                            data,
                        },
                    );
                }
                crate::agent::ProgressEvent::ToolResultDelta { .. } => {}
                crate::agent::ProgressEvent::Completed { response, turn_id, usage } => {
                    emit_event(
                        &tx,
                        super::GatewayEvent::Completed {
                            session_id: sid.clone(),
                            agent_id: aid.clone(),
                            response,
                            turn_id,
                            usage,
                        },
                    );
                }
                crate::agent::ProgressEvent::Error { message } => {
                    emit_event(
                        &tx,
                        super::GatewayEvent::ProcessingError {
                            session_id: sid.clone(),
                            agent_id: aid.clone(),
                            message,
                            code: None,
                        },
                    );
                }
            }
        })
    });

    let think_level = {
        let s = state.infra.runtime_settings.read().await;
        s.get("think.level")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
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

    agent.set_model_override(model_override).await;
    agent.set_extra_params(extra).await;

    let incoming_msg = crate::channels::IncomingMessage::new(
        user_id.to_string(),
        session_id.to_string(),
        message.to_string(),
    );

    let result = agent
        .process_message_with_progress(incoming_msg, progress_cb)
        .await;
    agent.set_model_override(None).await;

    match result {
        Ok(mut outgoing) => {
            let reasoning_vis = {
                let s = state.infra.runtime_settings.read().await;
                s.get("reasoning.visibility")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            };
            if reasoning_vis.as_deref() == Some("off") {
                outgoing.reasoning_content = None;
            }

            if let Some(ref usage) = outgoing.usage {
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

            let run_id = uuid::Uuid::new_v4().to_string();
            if let Some(ref store) = state.agents.store {
                if let Err(e) = store
                    .append_message(&AppendMessageParams {
                        session_id,
                        role: "assistant",
                        content: &outgoing.content,
                        transcript_id: Some(session_id),
                        run_id: Some(&run_id),
                        ..Default::default()
                    })
                    .await
                {
                    warn!("Failed to save assistant message to session history: {}", e);
                }
            }

            emit_event(
                &state.events.tx,
                super::GatewayEvent::AgentResponse {
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    content: outgoing.content,
                    channel: channel.to_string(),
                    conversation_id: session_id.to_string(),
                    usage: outgoing.usage,
                },
            );
        }
        Err(e) => {
            error!("Agent {} failed to process: {}", agent_id, e);
        }
    }
}

// ── create_default_tool_registry ─────────────────────────────────────────────

/// Dependencies required to build the default tool registry.
pub(crate) struct ToolRegistryArgs {
    /// ACP control plane for subagent/session tools.
    pub acp: Arc<AcpControlPlane>,
    /// MCP manager for MCP-backed tools.
    pub mcp_manager: Arc<McpManager>,
    /// Shared approval queue.
    pub approval_queue: Arc<ApprovalQueue>,
    /// Shared ask-user queue.
    pub ask_queue: Arc<AskQueue>,
    /// Persistent session store.
    pub session_store: Option<Arc<crate::agent::session_store::SessionStore>>,
    /// Lazy memory manager holder.
    pub memory_manager: Arc<RwLock<Option<Arc<crate::memory::MemoryManager>>>>,
    /// Capability flags.
    pub capabilities: CapabilitiesConfig,
    /// Audit logger.
    pub audit_log: Arc<dyn crate::security::runtime_audit::AuditLogger>,
    /// Optional content filter.
    pub content_filter: Option<Arc<crate::security::content_filter::ContentFilter>>,
    /// Search provider configuration.
    pub search_config: crate::gateway::config::SearchConfig,
    /// Syscity Cloud config (feature `cloud`) — used to register the cloud
    /// search provider when enabled.
    #[cfg(feature = "cloud")]
    pub cloud_config: crate::cloud::config::CloudConfig,
    /// Native device bridge (mobile only; `None` on desktop).
    pub device_bridge: Option<Arc<dyn crate::device::DeviceBridge>>,
    /// Shared skill manager for the on-demand `skill` tool.
    pub skills_manager: Arc<RwLock<crate::skills::SkillManager>>,
    /// Shell-hook `ToolHooks` bundle (empty when no hooks.json configured).
    pub tool_hooks: crate::tools::hooks::ToolHooks,
    /// Secret-store instance handle shared with the gateway (cloud tools).
    pub secrets: Arc<crate::secrets::SecretStoreHandle>,
    /// Layout root shared with the gateway.
    pub paths: Arc<crate::dirs::SyscityPaths>,
}

/// Create default tool registry with all built-in tools
pub(crate) async fn create_default_tool_registry(
    args: ToolRegistryArgs,
) -> crate::Result<ToolRegistry> {
    use crate::tools::*;

    let ToolRegistryArgs {
        acp,
        mcp_manager,
        approval_queue,
        ask_queue,
        session_store,
        memory_manager,
        capabilities,
        audit_log,
        content_filter,
        search_config,
        #[cfg(feature = "cloud")]
        cloud_config,
        device_bridge,
        skills_manager,
        tool_hooks,
        secrets,
        paths,
    } = args;

    let mut registry = ToolRegistry::new()
        .with_approval_queue(approval_queue)
        .with_ask_queue(ask_queue)
        .with_audit_log(audit_log)
        .with_hooks(tool_hooks);
    if let Some(filter) = content_filter {
        registry = registry.with_content_filter(filter);
    }

    // Register file system tools. The shared WriteGuard enforces
    // read-before-write and rejects stale overwrites per conversation.
    let write_guard = Arc::new(crate::tools::WriteGuard::new());
    registry.register(Box::new(FileReadTool::new().with_write_guard(write_guard.clone())));
    registry.register(Box::new(FileWriteTool::new().with_write_guard(write_guard.clone())));
    registry.register(Box::new(FileEditTool::new().with_write_guard(write_guard)));
    registry.register(Box::new(crate::tools::WriteReportTool::new(paths.clone())));
    registry.register(Box::new(crate::tools::SvgToPngTool::new(paths.clone())));
    registry.register(Box::new(GlobTool::new()));
    registry.register(Box::new(GrepTool::new()));

    // On-demand skill body loader (the prompt carries only the catalog).
    registry.register(Box::new(SkillTool::new(skills_manager)));

    // Register shell/execution tools wrapped in sandbox for path & timeout
    // enforcement. ShellTool needs network access (git, curl, etc.);
    // CodeExecutionTool does not.
    registry.register(Box::new(SandboxedTool::new(
        ShellTool::new(),
        SandboxConfig {
            allow_network_access: true,
            ..SandboxConfig::default()
        },
    )));
    registry.register(Box::new(SandboxedTool::new(
        CodeExecutionTool::default(),
        SandboxConfig::default(),
    )));

    // Register web tools. When cloud is enabled, prepend the cloud search
    // provider so logged-in sessions search via `/v1/search` (session token
    // resolved dynamically) — no local search key needed.
    #[cfg(feature = "cloud")]
    let mut search_providers = search_config.to_providers();
    #[cfg(not(feature = "cloud"))]
    let search_providers = search_config.to_providers();
    #[cfg(feature = "cloud")]
    if cloud_config.enabled {
        search_providers.insert(
            0,
            crate::tools::web::SearchProvider::Cloud {
                api_base: cloud_config.api_base.clone(),
            },
        );
    }
    let shared_providers = std::sync::Arc::new(tokio::sync::RwLock::new(search_providers));
    registry = registry.with_web_search_providers(shared_providers.clone());
    registry.register(Box::new(
        WebSearchTool::new()
            .with_providers_arc(shared_providers)
            .with_secrets(secrets.clone()),
    ));
    registry.register(Box::new(WebFetchTool::new()));

    // Cloud knowledge base tool (feature `cloud`): list/query/upload cloud KBs.
    #[cfg(feature = "cloud")]
    if cloud_config.enabled {
        registry.register(Box::new(crate::tools::cloud_kb::CloudKbTool::new(
            cloud_config.clone(),
            secrets.clone(),
        )));
    }

    // Register todo tool. The tool and the registry share one TodoState so
    // the engine can clear a conversation's active plan at each new turn.
    let todo_state = std::sync::Arc::new(TodoState::new());
    registry = registry.with_todo_state(todo_state.clone());
    registry.register(Box::new(TodoTool::with_state(todo_state)));

    // Register cron tool
    registry.register(Box::new(CronTool::new()));

    // Register heartbeat tool (agent self-management)
    registry.register(Box::new(HeartbeatTool::new()));

    // Register time tool
    registry.register(Box::new(TimeTool::new()));

    // Register browser tool (if browser feature enabled)
    #[cfg(feature = "browser")]
    registry.register(Box::new(BrowserTool::new()));

    // Register ACP tools for subagent spawning
    registry.register(Box::new(AcpSpawnTool::new(acp.clone(), session_store.clone())));
    registry.register(Box::new(AcpSessionTool::new(acp.clone())));

    // Register session tools
    registry.register(Box::new(SessionsListTool::new(session_store.clone())));
    registry.register(Box::new(SessionsHistoryTool::new(session_store.clone())));
    registry.register(Box::new(SessionsSendTool::new(acp.clone())));
    registry.register(Box::new(SessionsYieldTool::new(acp.clone())));
    registry.register(Box::new(SessionStatusTool::new(session_store.clone())));
    registry.register(Box::new(ApplyPatchTool::new()));

    // Register memory tool for persistent memory storage
    match MemoryTool::new(&paths).await {
        Ok(memory_tool) => {
            registry.register(Box::new(memory_tool));
            info!("MemoryTool registered successfully");
        }
        Err(e) => {
            warn!(
                "Failed to initialize MemoryTool: {}. Memory functionality will not be available.",
                e
            );
        }
    }

    // Register semantic/hybrid memory search tool
    match MemorySearchTool::new(&paths).await {
        Ok(tool) => {
            let tool = tool.with_manager_holder(memory_manager);
            registry.register(Box::new(tool));
            info!("MemorySearchTool registered successfully");
        }
        Err(e) => {
            warn!("Failed to initialize MemorySearchTool: {}. Hybrid search unavailable.", e);
        }
    }

    // Register memory get/CRUD tool
    match MemoryGetTool::new(&paths).await {
        Ok(tool) => {
            registry.register(Box::new(tool));
            info!("MemoryGetTool registered successfully");
        }
        Err(e) => {
            warn!("Failed to initialize MemoryGetTool: {}. Memory CRUD unavailable.", e);
        }
    }

    // Register MCP (Model Context Protocol) connection tool (uses shared manager)
    registry.register(Box::new(crate::mcp::McpConnectionTool::with_manager(mcp_manager)));

    // Register plan management tool
    registry.register(Box::new(UpdatePlanTool::new()));

    // Register process management tool
    registry.register(Box::new(ProcessTool::new()));

    // Register PDF generation tool
    registry.register(Box::new(PdfTool::new()));

    // Register image tools
    registry.register(Box::new(ImageTool::new()));
    registry.register(Box::new(ImageGenerateTool::new()));

    // Register TTS tool
    registry.register(Box::new(TtsTool::new()));

    // Register STT tool
    registry.register(Box::new(SttTool::new()));

    // Register nodes/Tailscale tool
    registry.register(Box::new(NodesTool::new()));

    // Register capability discovery tool
    registry.register(Box::new(ListCapabilitiesTool::new()));

    // Register ask-user tool (interactive clarification; the queue rides in
    // ToolContext so no queue reference is needed here).
    registry.register(Box::new(AskUserTool));

    // ── Device capability tools (mobile only; bridge is None on desktop,
    //    so is_available() is false and these are invisible to the agent) ──
    registry.register(Box::new(crate::device::DeviceCameraTool::new(device_bridge.clone())));
    registry.register(Box::new(crate::device::DeviceGeolocateTool::new(device_bridge.clone())));
    registry.register(Box::new(crate::device::DeviceNotifyTool::new(device_bridge.clone())));
    registry.register(Box::new(crate::device::DeviceHapticTool::new(device_bridge.clone())));
    registry.register(Box::new(crate::device::DevicePickFileTool::new(device_bridge.clone())));
    registry.register(Box::new(crate::device::DeviceShortcutRunTool::new(device_bridge.clone())));
    registry
        .register(Box::new(crate::device::DeviceShortcutResultsTool::new(device_bridge.clone())));
    registry.register(Box::new(crate::device::DeviceShortcutInboxTool::new(device_bridge)));

    // ── Register platform-specific capability sets ──
    {
        use crate::computer::platform::{
            CapabilityProfile, OsControlScope, PlatformCapabilityRegistry, ToolConflictStrategy,
        };

        let mut tool_reg = PlatformCapabilityRegistry::new();

        #[cfg(target_os = "linux")]
        {
            tool_reg.register(Box::new(crate::computer::platform::LinuxToolset::new()));
            tool_reg.register(Box::new(crate::computer::platform::LinuxDesktopX11Toolset::new()));
            tool_reg
                .register(Box::new(crate::computer::platform::LinuxDesktopWaylandToolset::new()));
        }

        #[cfg(target_os = "macos")]
        {
            tool_reg.register(Box::new(crate::computer::platform::MacosToolset::new()));
        }

        #[cfg(target_os = "windows")]
        {
            tool_reg.register(Box::new(crate::computer::platform::WindowsToolset::new()));
        }

        // On mobile (Android/iOS) the host OS tool sets are absent; the
        // Android ADB bridge targets the phone itself via the bundled adb
        // client (loopback self-pairing, §4.5). Availability is still gated
        // at runtime by `AndroidToolset::is_available()` (bundled adb present).
        #[cfg(mobile_os)]
        {
            tool_reg.register(Box::new(crate::computer::platform::AndroidToolset::new()));
        }

        // Load capability profile from config
        let profile = match capabilities.profile.as_str() {
            "minimal" => CapabilityProfile::Minimal,
            "observer" => CapabilityProfile::Observer,
            "server" => CapabilityProfile::Server,
            "desktop" => CapabilityProfile::Desktop,
            "custom" => CapabilityProfile::Custom(capabilities.custom_sets.clone()),
            _ => CapabilityProfile::Full,
        };
        let max_scope = match capabilities.max_scope.as_str() {
            "read_only" => Some(OsControlScope::ReadOnly),
            "user_space" => Some(OsControlScope::UserSpace),
            "system" => Some(OsControlScope::System),
            "root" => Some(OsControlScope::Root),
            _ => None,
        };
        let disabled_sets: std::collections::HashSet<String> =
            capabilities.disabled_sets.iter().cloned().collect();

        profile.apply(&mut tool_reg);

        // Apply max_scope filter: disable sets whose scope exceeds the limit
        if let Some(limit) = max_scope {
            let to_disable: Vec<String> = tool_reg
                .all_sets()
                .iter()
                .filter(|s| s.scope() > limit)
                .map(|s| s.id().to_string())
                .collect();
            for id in to_disable {
                tool_reg.disable(&id);
            }
        }

        // Apply explicit disabled_sets filter
        for id in &disabled_sets {
            tool_reg.disable(id);
        }

        // Log detected capabilities before exporting
        let available = tool_reg.available_sets();
        if available.is_empty() {
            info!("No platform-specific tool sets detected on this host");
        } else {
            for set in &available {
                info!(
                    "Platform tool set available: {} ({}) — {}",
                    set.name(),
                    set.id(),
                    set.description()
                );
            }
        }

        tool_reg.export_to_tool_registry(&mut registry, ToolConflictStrategy::Reject);

        info!("Platform tool sets exported: {} set(s) active", available.len());
    }

    // Gate high-privilege tools behind SkillTrust::Trusted.
    // Community-trust skills see only read-only / informational tools.
    registry.mark_privileged("shell");
    registry.mark_privileged("execute_code");
    registry.mark_privileged("file_write");
    registry.mark_privileged("file_edit");
    registry.mark_privileged("delegate");
    registry.mark_privileged("acp_spawn");
    registry.mark_privileged("acp_session");
    registry.mark_privileged("memory");
    registry.mark_privileged("sessions_send");
    registry.mark_privileged("sessions_yield");
    registry.mark_privileged("subagents");
    registry.mark_privileged("apply_patch");
    registry.mark_privileged("message");
    registry.mark_privileged("process");
    registry.mark_privileged("image_generate");

    // OS control tools — privileged because they modify system state.
    registry.mark_privileged("system_inspect");
    registry.mark_privileged("service_manager");

    // Device tools that touch sensitive hardware / user data.
    registry.mark_privileged("device_camera");
    registry.mark_privileged("device_geolocate");
    // Launching a user shortcut drives external app hand-off (read-only
    // results/inbox consumption stays unprivileged).
    registry.mark_privileged("ios_shortcut_run");

    Ok(registry)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// `AgentCommand::UpdateConfig` must reach the running agent's runtime
    /// config, not just the display copy on the handle. Otherwise hot-reload /
    /// `config.set agent_overrides.*` would appear to succeed without taking
    /// effect.
    #[tokio::test]
    async fn update_config_command_reaches_agent() {
        let state = std::sync::Arc::new(
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await,
        );

        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::mock::MockProvider::new());
        let tools = Arc::new(crate::tools::ToolRegistry::new());
        let agent_config = crate::agent::AgentConfig::default();
        let agent = Arc::new(crate::agent::Agent::new(agent_config.clone(), provider, tools));

        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let (query_tx, query_rx) = tokio::sync::mpsc::channel::<crate::gateway::AgentQuery>(32);
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let handle = crate::gateway::AgentHandle {
            id: "agent-a".to_string(),
            config: agent_config.clone(),
            base_config: agent_config,
            tx: tx.clone(),
            query_tx: query_tx.clone(),
            busy: busy.clone(),
            agent: agent.clone(),
        };
        state
            .agents
            .agents
            .write()
            .await
            .insert("agent-a".to_string(), handle);

        let loop_state = state.clone();
        let loop_agent_id = "agent-a".to_string();
        let loop_agent = agent.clone();
        let loop_busy = busy.clone();
        let loop_task = tokio::spawn(async move {
            run_agent_loop(loop_state, loop_agent_id, loop_agent, loop_busy, rx, query_rx, true)
                .await;
        });

        let mut new_config = crate::agent::AgentConfig::default();
        new_config.temperature = 0.9;
        tx.send(crate::gateway::AgentCommand::UpdateConfig(new_config))
            .await
            .unwrap();

        // The loop processes the update asynchronously; poll for the effect.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if agent.config_snapshot().temperature == 0.9 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("UpdateConfig did not reach the running agent's config");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        tx.send(crate::gateway::AgentCommand::Shutdown)
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_task).await;
    }
}
