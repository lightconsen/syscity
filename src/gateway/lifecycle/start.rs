//! Gateway startup: `start_gateway`, its background loops and the
//! delegation event forwarder.

use super::helpers::{init_mcp_servers, run_quality_gate_check, spawn_agent_in_lifecycle};
use super::*;

// ── start ────────────────────────────────────────────────────────────

/// Warn about webhook channels that will refuse every request at runtime.
///
/// The webhook handlers fail closed (see `src/gateway/webhooks.rs`): a channel
/// without its verification secret — or, for the pairing-style channels, with
/// `dm_policy` left open — is a configuration that *looks* enabled but cannot
/// be verified. This is not a refusal (the runtime does that per request); it
/// is the loud signal that the operator's first symptom is about to be
/// "webhooks stopped working".
fn warn_on_secretless_webhook_channels(config: &GatewayConfig) {
    for (name, channel) in &config.channels {
        if !channel.enabled {
            continue;
        }
        // The credential the webhook handler resolves (and now requires).
        let has_secret = match channel.channel_type {
            crate::channels::ChannelType::Whatsapp => {
                channel.credentials.contains_key("app_secret")
            }
            crate::channels::ChannelType::Slack => {
                channel.credentials.contains_key("signing_secret")
            }
            crate::channels::ChannelType::Feishu => {
                // Feishu resolves via the secret store first (runtime check);
                // the plaintext map is only the legacy fallback, so warn on
                // its absence but say why it might not matter.
                channel.credentials.contains_key("webhook_secret")
                    || channel.credentials.contains_key("secret")
            }
            _ => continue,
        };
        if has_secret {
            continue;
        }
        warn!(
            "channel '{name}' ({:?}) is enabled but has no webhook verification secret configured — \
             every webhook request to it will be refused until one is set",
            channel.channel_type,
        );
    }
}

/// Forward delegation task changes from the store's sink channel onto the
/// gateway event bus as `GatewayEvent::DelegationTaskUpdated`.
///
/// The sink carries task ids only; each id is re-read here so the event is
/// built from the row as it exists now (a burst of writes between sends
/// collapses into one fresh event). A row whose `parent_session` does not
/// resolve to a user session — rows written before the column existed, or a
/// broken chain — is skipped: there is no session to route it to, and legacy
/// rows are swept to `failed` at startup anyway.
pub(crate) async fn delegation_event_forwarder(
    store: Arc<crate::delegation::DelegationTaskStore>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    event_tx: tokio::sync::broadcast::Sender<GatewayEvent>,
    shutdown_token: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                info!("Delegation forwarder received shutdown signal, exiting");
                break;
            }
            changed = rx.recv() => {
                let Some(task_id) = changed else { break };
                let task = match store.get_task(&task_id).await {
                    Ok(Some(task)) => task,
                    Ok(None) => continue,
                    Err(e) => {
                        warn!("Failed to re-read delegation task '{}': {}", task_id, e);
                        continue;
                    }
                };
                let session_id = match store.root_session_for_task(&task_id).await {
                    Ok(Some(sid)) => sid,
                    Ok(None) => {
                        debug!(
                            "Delegation task '{}' has no root user session; not forwarded",
                            task_id
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!("Failed to resolve root session for '{}': {}", task_id, e);
                        continue;
                    }
                };
                let event = crate::gateway::GatewayEvent::DelegationTaskUpdated {
                    session_id,
                    task: crate::delegation::DelegationTaskSnapshot::from_task(
                        &task,
                        delegation_duration_ms(&task),
                    ),
                };
                if let Err(e) = event_tx.send(event) {
                    debug!("No receivers for DelegationTaskUpdated event: {}", e);
                }
            }
        }
    }
}

/// `completed_at − created_at` in milliseconds when the task is terminal.
/// Unparseable timestamps yield `None` — never a silent zero, which would
/// read as "finished instantly".
fn delegation_duration_ms(task: &crate::delegation::DelegationTask) -> Option<u64> {
    let created = chrono::DateTime::parse_from_rfc3339(&task.created_at).ok()?;
    let completed = chrono::DateTime::parse_from_rfc3339(task.completed_at.as_deref()?).ok()?;
    let ms = (completed - created).num_milliseconds();
    if ms < 0 {
        None
    } else {
        Some(ms as u64)
    }
}

/// Start the gateway and all its subsystems.
pub(crate) async fn start_gateway(
    state: Arc<GatewayState>,
    config: GatewayConfig,
    shutdown_token: CancellationToken,
) -> crate::Result<()> {
    info!("Starting Syscity Gateway control plane...");

    // Refuse to serve a configuration that leaves the control plane open, and
    // say so loudly when it is merely risky. This has to happen before the
    // listener exists: the point is to not come up at all.
    crate::gateway::validate_auth_config(&config)?;

    // A webhook channel that does not carry its verification secret will
    // refuse every request at runtime — the handlers fail closed. Say so at
    // startup rather than letting the operator learn it from "webhooks stopped
    // working".
    warn_on_secretless_webhook_channels(&config);

    // ── MCP presets: auto-create mcp.toml with defaults if missing ──
    {
        let mcps_path = state.paths.config_dir().join("mcp.toml");
        if !mcps_path.exists() {
            if let Err(e) = tokio::fs::write(&mcps_path, crate::mcp::DEFAULT_PRESETS_TOML).await {
                warn!("Failed to create default MCP presets file: {e}");
            } else {
                info!("Created default MCP presets at {}", mcps_path.display());
            }
        }
    }

    // Initialize plugins if enabled
    if config.plugins.enabled {
        if config.plugins.auto_load {
            if let Err(e) = state.infra.plugin_manager.initialize().await {
                warn!("Failed to initialize plugins: {}", e);
            }

            // Watch WASM files for hot-reload
            if let Some(hot_reload) = state.infra.hot_reload.read().await.clone() {
                let plugins = state.infra.plugin_manager.list_plugins().await;
                for plugin in plugins {
                    if let Some(ref main) = plugin.manifest.main {
                        let wasm_path = plugin.path.join(main);
                        if wasm_path.exists() {
                            if let Err(e) = hot_reload
                                .watch_file(&wasm_path, ConfigFileType::Plugin)
                                .await
                            {
                                warn!(
                                    "Failed to watch WASM file for plugin '{}': {}",
                                    plugin.id(),
                                    e
                                );
                            } else {
                                debug!(
                                    "Watching WASM file for plugin '{}': {:?}",
                                    plugin.id(),
                                    wasm_path
                                );
                            }
                        }
                    }
                }
            }
        } else {
            info!("Plugin auto-load disabled, skipping initialization");
        }
    } else {
        info!("Plugin system disabled");
    }

    // Initialize skills manager
    {
        let mut skills_manager = state.tools.skills_manager.write().await;
        match skills_manager.initialize().await {
            Ok(count) => info!("✅ Skills manager initialized with {} skills", count),
            Err(e) => warn!("Failed to initialize skills manager: {}", e),
        }
    }

    // Start model-router health checks. They are registered with the task
    // registry and respect the gateway shutdown token.
    state.infra.model_router.clone().start_health_checks();

    // Register the cloud model provider when cloud is enabled (§2.7). The
    // provider's credential is a store ref to the session token, so it
    // resolves dynamically once the user logs in — no rebuild needed. The
    // model list here is only the seed; it is refreshed from cloud
    // `/v1/models` (see models.list). Update-or-add so a stale persisted
    // "cloud" entry cannot make startup fail.
    #[cfg(feature = "cloud")]
    {
        let cloud_cfg = state.config.read().await.cloud.clone();
        if cloud_cfg.enabled {
            let cfg = crate::cloud::provider::provider_config(&cloud_cfg, &[]);
            let res = if state.infra.model_router.provider_exists("cloud").await {
                state.infra.model_router.update_provider("cloud", cfg).await
            } else {
                state.infra.model_router.add_provider("cloud", cfg).await
            };
            match res {
                Ok(()) => info!("Registered cloud model provider (login to use)"),
                Err(e) => warn!("Failed to register cloud model provider: {e}"),
            }
        }
    }

    // Initialize hot reload if enabled
    let hot_reload = state.infra.hot_reload.read().await.clone();
    if let Some(ref hot_reload) = hot_reload {
        let config_path = state.paths.default_config_file();
        if let Err(e) = hot_reload
            .watch_file(&config_path, ConfigFileType::Main)
            .await
        {
            warn!("Failed to watch config file: {}", e);
        }
        // Start hot reload processing in background
        let hot_reload_clone = hot_reload.clone();
        let hot_reload_shutdown = shutdown_token.clone();
        let hot_reload_handle = tokio::spawn(async move {
            // `run()` parks on the file-watcher's channel, and the watcher
            // holds the sender for the life of the process, so it never
            // returns on its own — without the token this task would sit there
            // until shutdown aborted it.
            tokio::select! {
                res = hot_reload_clone.run() => {
                    if let Err(e) = res {
                        error!("Hot reload error: {}", e);
                    }
                }
                _ = hot_reload_shutdown.cancelled() => {}
            }
        });
        state
            .task_registry
            .insert_join("hot_reload", hot_reload_handle)
            .await;

        // Register config change handlers
        crate::gateway::hot_reload::register_hot_reload_handlers(
            state.clone(),
            config.clone(),
            hot_reload,
        )
        .await;
    }

    // Initialize default agent (optional - requires provider configuration)
    let default_config =
        crate::gateway::augment_default_agent_config(&config.default_agent, &state.paths);
    match spawn_agent_in_lifecycle(state.clone(), "default".to_string(), default_config).await {
        Ok(()) => info!("Default agent spawned successfully"),
        Err(e) => {
            warn!("Failed to spawn default agent: {}", e);
            warn!("Gateway running without default agent - agents must be created via API");
        }
    }

    // Discover agents from agents/ directory (auto-discovery)
    {
        let mut registry = state.agents.registry.write().await;
        match registry.discover(&state.paths).await {
            Ok(count) => {
                if count > 0 {
                    info!("🔍 Discovered {} agents from agents/ directory", count);
                    // List discovered agents
                    for id in registry.list() {
                        if let Some(personality) = registry.get(&id) {
                            info!("  📋 Agent '{}' - {}", id, personality.display_name());
                        }
                    }
                } else {
                    info!("🔍 No agents found in agents/ directory");
                }
            }
            Err(e) => {
                warn!("Failed to discover agents: {}", e);
            }
        }
    }

    // Watch kb.toml files for hot-reload
    if let Some(ref hot_reload) = *state.infra.hot_reload.read().await {
        use crate::config::hot_reload::ConfigFileType;
        let agents_dir = state.paths.agents_dir();
        if agents_dir.exists() {
            let mut read_dir = match tokio::fs::read_dir(&agents_dir).await {
                Ok(d) => d,
                Err(e) => {
                    warn!("Failed to read agents dir for KB watching: {}", e);
                    return Ok(());
                }
            };
            while let Some(entry) = read_dir.next_entry().await.unwrap_or(None) {
                let kb_toml = entry.path().join("kb.toml");
                if kb_toml.exists() {
                    if let Err(e) = hot_reload
                        .watch_file(&kb_toml, ConfigFileType::KnowledgeBase)
                        .await
                    {
                        warn!("Failed to watch kb.toml: {:?} - {}", kb_toml, e);
                    } else {
                        info!(
                            "Watching kb.toml for agent '{}'",
                            entry
                                .path()
                                .file_name()
                                .map(|n| n.to_string_lossy())
                                .unwrap_or_default()
                        );
                    }
                }
            }
        }
    }

    // Register delegation tool with agent resolver for target_agent routing,
    // plus the shared task-state store, the `task_state` tool for delegation
    // trees (children read/write their shared state via that tool), and a
    // handoff coordinator for successor continuation.
    {
        use crate::delegation::{
            AgentWakeHandler, DelegationCoordinator, DelegationTaskStore, DelegationWake,
            TaskStateTool,
        };
        use crate::tools::DelegateTool;

        let db_url =
            format!("sqlite://{}", state.paths.data_dir().join("delegations.db").display());
        // Task changes travel store → sink channel → forwarder → gateway event
        // bus → WS clients. The sink carries task ids only; the forwarder
        // re-reads each row so events always carry fresh data.
        let (deleg_tx, deleg_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let delegation_store = Arc::new(
            DelegationTaskStore::new(&db_url)
                .await?
                .with_event_sink(deleg_tx),
        );
        let delegation_store_for_forwarder = delegation_store.clone();
        // Sweep rows left in-flight by a previous process: they belong to
        // executions that died with it and would otherwise read as "running"
        // forever.
        match delegation_store.fail_orphaned_runs().await {
            Ok(0) => {}
            Ok(n) => warn!("Marked {} orphaned delegation task(s) as failed", n),
            Err(e) => warn!("Failed to sweep orphaned delegation tasks: {}", e),
        }
        state
            .tools
            .registry
            .register_dynamic(Arc::new(TaskStateTool::new(
                delegation_store.clone(),
                state.paths.clone(),
            )));

        let resolver: Arc<dyn crate::tools::delegate_tool::AgentResolver> =
            Arc::new(crate::gateway::agent_spawn::GatewayAgentResolver {
                agents: state.agents.agents.clone(),
            });
        let default_agent = {
            let agents = state.agents.agents.read().await;
            agents.get("default").map(|h| h.agent.clone())
        };

        // Parent auto-wake (v2): when a child completes after its parent's turn
        // ended, wake the parent's session with the child's result so it can
        // aggregate.  The resolver maps a parent session key to the agent that
        // owns it (router-bound user sessions for tree roots, the task row's
        // agent_id for delegated parents).
        let wake = Arc::new(DelegationWake::new(Arc::new(AgentWakeHandler::new(Arc::new(
            crate::gateway::agent_spawn::GatewayWakeResolver {
                agents: state.agents.agents.clone(),
                router: state.agents.router.clone(),
                store: delegation_store.clone(),
            },
        )))));

        let delegate = if let Some(agent) = default_agent.clone() {
            DelegateTool::with_agent(0, agent)
                .with_agent_resolver(Arc::clone(&resolver))
                .with_task_store(delegation_store.clone())
                .with_wake(wake.clone())
        } else {
            DelegateTool::root()
                .with_agent_resolver(Arc::clone(&resolver))
                .with_task_store(delegation_store.clone())
                .with_wake(wake)
        };
        let coordinator = Arc::new(DelegationCoordinator::new(
            delegation_store,
            delegate.registry().clone(),
            resolver,
            default_agent,
        ));
        state
            .tools
            .registry
            .register_dynamic(Arc::new(delegate.with_coordinator(coordinator)));
        info!("DelegateTool registered with agent resolver for target_agent routing");

        // Push delegation task changes onto the gateway event bus so WS
        // clients (TUI, web) can render live task rows.
        {
            let event_tx = state.events.tx.clone();
            let shutdown_token = shutdown_token.clone();
            let handle = tokio::spawn(delegation_event_forwarder(
                delegation_store_for_forwarder,
                deleg_rx,
                event_tx,
                shutdown_token,
            ));
            state
                .task_registry
                .insert_join("delegation_event_forwarder", handle)
                .await;
        }
    }

    // Auto-connect MCP servers (non-blocking — HTTP listener starts immediately)
    init_mcp_servers(state.clone(), &config);

    // Reconnect enabled connectors (MCP-backed ones) in the background.
    {
        let connectors = state.tools.connector_manager.clone();
        let task_registry = state.task_registry.clone();
        let handle = tokio::spawn(async move {
            let n = connectors.load_and_connect().await;
            if n > 0 {
                info!("{n} connector(s) reconnected after restart");
            }
        });
        task_registry
            .insert_join("connectors:load_and_connect", handle)
            .await;
    }

    // Initialize configured channels
    crate::gateway::init::channels::init_channels(state.clone(), &config).await?;

    // Start dream scheduler if enabled
    if config.dreaming.enabled {
        if let Some(mm) = state.memory.manager.read().await.as_ref().cloned() {
            if let Some(tier_index) = mm.tier_index() {
                let dreaming = &config.dreaming;
                let speed = match dreaming.speed.to_lowercase().as_str() {
                    "fast" => crate::memory::DreamSpeed::Fast,
                    "slow" => crate::memory::DreamSpeed::Slow,
                    _ => crate::memory::DreamSpeed::Balanced,
                };
                let thinking = match dreaming.thinking.to_lowercase().as_str() {
                    "low" => crate::memory::DreamThinking::Low,
                    "high" => crate::memory::DreamThinking::High,
                    _ => crate::memory::DreamThinking::Medium,
                };
                let budget = match dreaming.budget.to_lowercase().as_str() {
                    "cheap" => crate::memory::DreamBudget::Cheap,
                    "expensive" => crate::memory::DreamBudget::Expensive,
                    _ => crate::memory::DreamBudget::Medium,
                };
                let dream_config = crate::memory::DreamConfig {
                    enabled: dreaming.enabled,
                    frequency: dreaming.frequency.clone(),
                    speed,
                    thinking,
                    budget,
                    dedup_similarity_threshold: dreaming.dedup_similarity_threshold,
                    ..crate::memory::DreamConfig::default()
                };
                let tier_system_config = crate::memory::TierSystemConfig::default();
                let mut engine = crate::memory::DreamEngine::new(dream_config, tier_system_config)
                    .with_metrics(Arc::clone(&state.memory.dream_metrics));
                if let Some(ref workspace_dir) = config.workspace_dir {
                    engine = engine.with_workspace_dir(workspace_dir.clone());
                }
                if let Some(event_log) = mm.event_log() {
                    engine = engine.with_event_log(event_log.clone());
                }
                engine.initialize().await;
                let engine = Arc::new(engine);
                let mut scheduler = crate::memory::DreamScheduler::new(engine);
                let handle = scheduler.start(mm.store(), tier_index);
                state
                    .task_registry
                    .insert_join("dream_scheduler", handle)
                    .await;
                info!("Dream scheduler started");
                *state.memory.dream_scheduler.write().await = Some(scheduler);
            }
        }
    }

    // Initialize standing orders manager if configured
    if config.standing_orders.enabled {
        let mut manager = crate::standing_orders::StandingOrderManager::new(
            config.standing_orders.clone(),
            state.clone(),
        );
        manager.start().await;
        info!("Standing orders manager started");
        *state.memory.standing_order_manager.write().await = Some(manager);
    }

    // Run quality gate check if enabled
    if config.quality_gate.enabled {
        if let Err(e) = run_quality_gate_check(state.clone(), &config).await {
            if config.quality_gate.shutdown_on_failure {
                return Err(e);
            }
        }
    }

    // Start the harness scalar optimizer scheduler (§十二 可调参). Only when
    // enabled AND a real cadence ("manual" runs are triggered on demand via
    // `eval.optimizer.run`). The loop honors the shutdown token and the
    // circuit-breaker pause flag (Phase 4 guardrail hook).
    if config.eval.optimizer.enabled {
        if let Some(cadence) = crate::eval::parse_cadence(&config.eval.optimizer.cadence) {
            let optimizer = crate::eval::ScalarOptimizer::new(state.infra.optimizer.clone());
            let run_state = state.clone();
            let shutdown = run_state.shutdown_token.clone();
            let handle = tokio::spawn(async move {
                let mut interval = tokio::time::interval(cadence);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => {
                            debug!("Scalar optimizer scheduler stopped");
                            break;
                        }
                        _ = interval.tick() => {
                            if run_state.infra.optimizer.paused
                                .load(std::sync::atomic::Ordering::SeqCst)
                            {
                                debug!("Scalar optimizer scheduler paused (circuit breaker)");
                                continue;
                            }
                            let report = optimizer
                                .run(run_state.clone(), crate::eval::OptimizerRunParams::default())
                                .await;
                            // Phase 4 guardrail: after an apply, re-check online
                            // signals and auto-rollback anything that degraded.
                            if !report.applied.is_empty() {
                                let cfg = run_state.config.read().await;
                                let guard = &cfg.eval.optimizer.guardrails;
                                let guard_enabled = guard.enabled;
                                let min_votes = guard.min_votes;
                                let max_online_risks = guard.max_online_risks;
                                let window_ms = (guard.window_hours as i64) * 3600 * 1000;
                                drop(cfg);
                                if guard_enabled {
                                    let evaluator = crate::eval::OnlineSignalShadowEvaluator::new(
                                        run_state.infra.feedback_store.clone(),
                                        run_state.infra.pending_badcase_store.clone(),
                                        0.3, // stricter floor for rollback decisions
                                        min_votes,
                                        max_online_risks,
                                        window_ms,
                                    );
                                    let anomalous = match evaluator.is_anomalous().await {
                                        Ok(v) => v,
                                        Err(e) => {
                                            warn!("Post-apply signal check failed: {e}");
                                            false
                                        }
                                    };
                                    if anomalous {
                                        for applied_patch in &report.applied {
                                            match optimizer
                                                .rollback(
                                                    run_state.clone(),
                                                    &applied_patch.path,
                                                    "online_anomaly",
                                                )
                                                .await
                                            {
                                                Ok(rb) => info!(
                                                    "Auto-rollback {} {}→{}: {}",
                                                    rb.subject, rb.from, rb.to, rb.reason
                                                ),
                                                Err(e) => warn!(
                                                    "Auto-rollback failed for {}: {}",
                                                    applied_patch.path, e
                                                ),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            });
            state
                .task_registry
                .insert_join("eval.optimizer.scheduler", handle)
                .await;
            info!(cadence_secs = cadence.as_secs(), "Scalar optimizer scheduler started");
        }
    }

    // Start browser bridge server if enabled
    #[cfg(feature = "browser")]
    if config.browser.bridge_enabled {
        let pool = Arc::new(crate::browser::BrowserPool::with_profiles(
            config.browser.pool.clone(),
            config.browser.profiles.clone(),
        ));
        let mut bridge = crate::browser::BrowserBridge::new(pool, config.browser.bridge_port);
        let token = bridge.token().to_string();
        match bridge.start().await {
            Ok(port) => {
                let url = format!("http://127.0.0.1:{}", port);
                info!(port = port, "Browser bridge server started");
                {
                    let mut bridge_lock = state.infra.browser_bridge.write().await;
                    *bridge_lock = Some(bridge);
                }
                let mut settings = state.infra.runtime_settings.write().await;
                settings.insert("browser_bridge_url".to_string(), serde_json::json!(url));
                settings.insert("browser_bridge_token".to_string(), serde_json::json!(token));
            }
            Err(e) => {
                warn!("Failed to start browser bridge server: {}", e);
            }
        }
    }

    // Build HTTP router
    let app = build_router(state.clone()).await;

    // Bind to address
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| crate::error::ConfigError::InvalidValue {
            key: "gateway.address".to_string(),
            message: format!("Invalid gateway address: {}", e),
        })?;

    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        crate::error::SyscityError::ExternalService {
            source: "Failed to bind gateway".to_string(),
            cause: Some(Box::new(e)),
        }
    })?;

    info!("Gateway control plane listening on ws://{}", addr);

    // Forward ApprovalRequired events from the tool registry into the Gateway event
    // bus
    {
        let mut approval_rx = state.tools.approval_queue.event_tx.subscribe();
        let event_tx = state.events.tx.clone();
        let shutdown_token = shutdown_token.clone();
        let approval_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        info!("Approval forwarder received shutdown signal, exiting");
                        break;
                    }
                    result = approval_rx.recv() => {
                        let evt = match result {
                            Ok(evt) => evt,
                            Err(_) => break,
                        };
                        if let Err(e) = event_tx.send(crate::gateway::GatewayEvent::ApprovalRequired {
                            approval_id: evt.approval_id,
                            tool_name: evt.tool_name,
                            requested_by: evt.requested_by,
                            risk_level: evt.risk_level,
                            message: evt.message,
                            session_id: evt.session_id,
                        }) {
                            debug!("No receivers for ApprovalRequired event: {}", e);
                        }
                    }
                }
            }
        });
        state
            .task_registry
            .insert_join("approval_forwarder", approval_handle)
            .await;
    }

    // Deny approvals whose waiter has gone away. `ApprovalQueue::cleanup_stale`
    // existed but had no caller, so an approval that nobody was left waiting on
    // stayed pending forever — visible to the UI, resolvable by nobody. The
    // timeout it enforces is the queue's own (`default_timeout`).
    {
        let queue = state.tools.approval_queue.clone();
        let shutdown_token = shutdown_token.clone();
        let sweeper = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(APPROVAL_SWEEP_INTERVAL);
            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        info!("Approval sweeper received shutdown signal, exiting");
                        break;
                    }
                    _ = ticker.tick() => {
                        let expired = queue.cleanup_stale().await;
                        if expired > 0 {
                            info!("Denied {} stale approval(s)", expired);
                        }
                    }
                }
            }
        });
        state
            .task_registry
            .insert_join("approval_sweeper", sweeper)
            .await;
    }

    // Forward ask_user events from the ask queue into the Gateway event bus.
    {
        let mut ask_rx = state.tools.ask_queue.event_tx.subscribe();
        let event_tx = state.events.tx.clone();
        let shutdown_token = shutdown_token.clone();
        let ask_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        info!("Ask forwarder received shutdown signal, exiting");
                        break;
                    }
                    result = ask_rx.recv() => {
                        let evt = match result {
                            Ok(evt) => evt,
                            Err(_) => break,
                        };
                        let gateway_evt = match evt {
                            crate::tools::ask_user::AskEvent::Required(e) => {
                                crate::gateway::GatewayEvent::AskRequired(e)
                            }
                            crate::tools::ask_user::AskEvent::Resolved(e) => {
                                crate::gateway::GatewayEvent::AskResolved(e)
                            }
                        };
                        if let Err(e) = event_tx.send(gateway_evt) {
                            debug!("No receivers for ask event: {}", e);
                        }
                    }
                }
            }
        });
        state
            .task_registry
            .insert_join("ask_forwarder", ask_handle)
            .await;
    }

    // Start gateway-level self-repair watchdog (60 s interval)
    let repair_handle = tokio::spawn(crate::gateway::watchdog::run_repair_loop(
        state.clone(),
        shutdown_token.clone(),
    ));
    state
        .task_registry
        .insert_join("repair_loop", repair_handle)
        .await;

    // Start heartbeat runner if enabled
    if config.heartbeat.enabled {
        let runner = crate::heartbeat::HeartbeatRunner::new(state.clone());
        let wake_tx = runner.wake_sender();
        let event_tx = runner.event_tx.clone();
        *state.scheduler.heartbeat_wake_tx.write().await = Some(wake_tx.clone());
        *state.scheduler.heartbeat_event_tx.write().await = Some(event_tx);
        let heartbeat_handle = tokio::spawn(async move {
            runner.start().await;
        });
        state
            .task_registry
            .insert_join("heartbeat", heartbeat_handle)
            .await;
        info!("Heartbeat runner started");

        // Wire heartbeat wake sender into cron scheduler
        if let Some(cron_arc) = state.scheduler.cron_scheduler.read().await.clone() {
            let mut scheduler = cron_arc.lock().await;
            scheduler.set_heartbeat_wake_tx(wake_tx);
            info!("Cron heartbeat wake integration enabled");
        }
    }

    // Start log tail broadcaster for real-time log streaming
    {
        let log_tx = state.events.log_tx.clone();
        let shutdown_token = shutdown_token.clone();
        let log_tail_handle = tokio::spawn(async move {
            let log_path = crate::logs::log_file_path();
            let mut pos: u64 = 0;
            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        info!("Log tail broadcaster received shutdown signal, exiting");
                        break;
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                        if log_path.exists() {
                            match tokio::fs::metadata(&log_path).await {
                                Ok(meta) => {
                                    let new_len = meta.len();
                                    if new_len > pos {
                                        match tokio::fs::File::open(&log_path).await {
                                            Ok(file) => {
                                                let mut reader = tokio::io::BufReader::new(file);
                                                if let Err(e) =
                                                    reader.seek(tokio::io::SeekFrom::Start(pos)).await
                                                {
                                                    tracing::warn!("Log tail seek error: {}", e);
                                                } else {
                                                    let mut lines = reader.lines();
                                                    while let Ok(Some(line)) = lines.next_line().await {
                                                        if let Err(e) = log_tx.send(line) {
                                                            debug!(
                                                                "No receivers for log tail event: {}",
                                                                e
                                                            );
                                                        }
                                                    }
                                                }
                                                pos = new_len;
                                            }
                                            Err(e) => {
                                                tracing::warn!("Log tail open error: {}", e);
                                            }
                                        }
                                    } else if new_len < pos {
                                        // File was truncated/rotated
                                        pos = 0;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Log tail metadata error: {}", e);
                                }
                            }
                        }
                    }
                }
            }
        });
        state
            .task_registry
            .insert_join("log_tail", log_tail_handle)
            .await;
        info!("Log tail broadcaster started");
    }

    // Persisted goals are deliberately NOT auto-resumed: a restart must not
    // re-arm autonomous loops without explicit human consent. Suspended
    // goals are listed by `/goal list` and re-armed by `/goal resume <id>`.
    {
        let goal_store = crate::goal::persist::GoalStore::new();
        let persisted = goal_store.load_all().await;
        if !persisted.is_empty() {
            info!(
                "{} persisted goal(s) suspended (not auto-resumed); resume with /goal resume <id>",
                persisted.len()
            );
        }
    }

    // Run the server until the shutdown token is cancelled.  Axum's
    // graceful-shutdown mechanism drains existing connections after the
    // token fires.  Stuck handles are aborted later by `stop_gateway`
    // (step 12 — abort remaining background tasks) so no separate drain
    // timeout is needed here.
    let serve =
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async move { shutdown_token.cancelled().await });
    serve
        .await
        .map_err(|e| crate::error::SyscityError::ExternalService {
            source: "Gateway server error".to_string(),
            cause: Some(Box::new(e)),
        })?;

    Ok(())
}
