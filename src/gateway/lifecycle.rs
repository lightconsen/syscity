//! Gateway lifecycle functions — start, stop, build_router.
//!
//! Extracted from `gateway/mod.rs` to reduce the main module size. Each
//! function takes the pieces of [`Gateway`](super::Gateway) it needs
//! explicitly (state, config, shutdown_token, task-trackers) instead of
//! `&self`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    middleware::{from_fn, from_fn_with_state},
    routing::{get, post},
    Router,
};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, warn};

use super::agent_spawn::spawn_agent_inner;
use super::*;
use super::{GatewayConfig, GatewayState};
use crate::agent::AgentConfig;
use crate::config::hot_reload::ConfigFileType;
use crate::mcp::McpToolWrapper;

// ── start ────────────────────────────────────────────────────────────

/// Start the gateway and all its subsystems.
pub(crate) async fn start_gateway(
    state: Arc<GatewayState>,
    config: GatewayConfig,
    shutdown_token: CancellationToken,
) -> crate::Result<()> {
    info!("Starting Syscity Gateway control plane...");

    // ── MCP presets: auto-create mcp.toml with defaults if missing ──
    {
        let mcps_path = crate::dirs::config_dir().join("mcp.toml");
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
    // resolves dynamically once the user logs in — no rebuild needed.
    #[cfg(feature = "cloud")]
    {
        let cloud_cfg = state.config.read().await.cloud.clone();
        if cloud_cfg.enabled {
            let cfg = crate::cloud::provider::provider_config(&cloud_cfg);
            match state.infra.model_router.add_provider("cloud", cfg).await {
                Ok(()) => info!("Registered cloud model provider (login to use)"),
                Err(e) => warn!("Failed to register cloud model provider: {e}"),
            }
        }
    }

    // Initialize hot reload if enabled
    let hot_reload = state.infra.hot_reload.read().await.clone();
    if let Some(ref hot_reload) = hot_reload {
        let config_path = crate::dirs::default_config_file();
        if let Err(e) = hot_reload
            .watch_file(&config_path, ConfigFileType::Main)
            .await
        {
            warn!("Failed to watch config file: {}", e);
        }
        // Start hot reload processing in background
        let hot_reload_clone = hot_reload.clone();
        let hot_reload_handle = tokio::spawn(async move {
            if let Err(e) = hot_reload_clone.run().await {
                error!("Hot reload error: {}", e);
            }
        });
        state
            .task_registry
            .insert_join("hot_reload", hot_reload_handle)
            .await;

        // Register config change handlers
        super::hot_reload::register_hot_reload_handlers(state.clone(), config.clone(), hot_reload)
            .await;
    }

    // Initialize default agent (optional - requires provider configuration)
    let default_config = super::augment_default_agent_config(&config.default_agent);
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
        match registry.discover().await {
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
        let agents_dir = crate::dirs::agents_dir();
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
            format!("sqlite://{}", crate::dirs::data_dir().join("delegations.db").display());
        let delegation_store = Arc::new(DelegationTaskStore::new(&db_url).await?);
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
            .register_dynamic(Arc::new(TaskStateTool::new(delegation_store.clone())));

        let resolver: Arc<dyn crate::tools::delegate_tool::AgentResolver> =
            Arc::new(super::agent_spawn::GatewayAgentResolver {
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
            super::agent_spawn::GatewayWakeResolver {
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
    super::init::channels::init_channels(state.clone(), &config).await?;

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
    let repair_handle =
        tokio::spawn(super::watchdog::run_repair_loop(state.clone(), shutdown_token.clone()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    #[tokio::test]
    async fn build_router_serves_live_endpoint() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = state().await;
        let app = build_router(state).await;

        let req = Request::builder().uri("/live").body(Body::empty()).unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["alive"], true);
    }

    #[tokio::test]
    async fn build_router_serves_cloud_login_callback() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = state().await;
        let app = build_router(state).await;

        // The cloud OAuth return URL (default cloud.redirect_base) must serve
        // the SPA so its App.tsx can read `#token=` and persist the session.
        let req = Request::builder()
            .uri("/cloud/login/callback")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("<html") || html.contains("Syscity"));
    }

    #[tokio::test]
    async fn build_router_serves_web_ws_route() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let state = state().await;
        let app = build_router(state).await;

        // The /ws route is registered (GET requires an upgrade; a plain GET
        // must be rejected with a non-panicking response).
        let req = Request::builder().uri("/ws").body(Body::empty()).unwrap();
        let response = app.oneshot(req).await.unwrap();
        // A missing upgrade should not produce a 200; any client error is fine.
        assert!(
            response.status().is_client_error() || response.status() == StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn stop_gateway_clean_shutdown() {
        let state = state().await;
        let token = CancellationToken::new();
        let result = stop_gateway(&token, &state).await;
        assert!(result.is_ok(), "stop_gateway should succeed: {:?}", result);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn init_mcp_servers_empty_config_noop() {
        let state = state().await;
        init_mcp_servers(state, &GatewayConfig::default());
    }

    #[tokio::test]
    async fn init_mcp_servers_skips_non_auto_connect() {
        let state = state().await;
        let mut config = GatewayConfig::default();
        let mut server = crate::mcp::McpServerConfig::default();
        server.auto_connect = false;
        config.mcp.servers.insert("ghost".to_string(), server);
        init_mcp_servers(state, &config);
    }

    #[tokio::test]
    async fn register_mcp_tools_without_client_noop() {
        let state = state().await;
        register_mcp_tools(&state, "ghost", &[], 0).await;
        let registry = state.tools.registry.clone();
        assert!(
            !registry
                .list()
                .iter()
                .any(|n| n.starts_with("mcp__ghost__")),
            "no MCP tools should be registered without a connected client"
        );
    }
}

// ── stop ─────────────────────────────────────────────────────────────

/// Gracefully shut down the gateway and its subsystems.
pub(crate) async fn stop_gateway(
    shutdown_token: &CancellationToken,
    state: &Arc<GatewayState>,
) -> crate::Result<()> {
    info!("Shutting down Syscity Gateway...");

    // Signal every cancel-aware loop to exit.
    shutdown_token.cancel();

    // 1. Drain the unified message workers.
    // Abort all first (shutdown_token was already cancelled), then await
    // so we don't hang on stuck tasks.
    let message_handles = state
        .task_registry
        .remove_matching_join_or_abort("message:")
        .await;
    for handle in &message_handles {
        handle.abort();
    }
    for handle in message_handles {
        let _ = handle.await;
    }

    // 2. Stop all spawned agents and await their loops.
    {
        let agents = state.agents.agents.read().await;
        for (id, handle) in agents.iter() {
            if let Err(e) = handle.tx.send(crate::gateway::AgentCommand::Shutdown).await {
                warn!("Failed to send shutdown to agent {}: {}", id, e);
            }
        }
    }
    let agent_handles = state
        .task_registry
        .remove_matching_join_or_abort("agent:")
        .await;
    for handle in &agent_handles {
        handle.abort();
    }
    for handle in agent_handles {
        let _ = handle.await;
    }

    // 2b. Abort running goal runners and their event relays ("goal:{id}" /
    // "goal-relay:{id}"). Shutdown must NOT take the cooperative-cancel
    // path: /goal cancel deletes the checkpoint (the user explicitly
    // discards the goal), while shutdown is crash-equivalent — the last
    // round checkpoint survives and the goal shows up as suspended after
    // restart.
    let goal_handles = state
        .task_registry
        .remove_matching_join_or_abort("goal")
        .await;
    for handle in &goal_handles {
        handle.abort();
    }
    for handle in goal_handles {
        let _ = handle.await;
    }

    // 3. Stop configured channels.
    // Abort channel background tasks first so the channel stop() calls do not
    // race with gateway-owned inbound/outbound bridges.
    let channel_handles = state
        .task_registry
        .remove_matching_join_or_abort("channel:")
        .await;
    for handle in channel_handles {
        handle.abort();
    }
    let channel_refs: Vec<Arc<dyn crate::channels::Channel>> = {
        let channels = state.channels.channels.read().await;
        channels.values().cloned().collect()
    };
    for channel in channel_refs {
        let name = channel.name().to_string();
        if let Err(e) = channel.stop().await {
            warn!("Failed to stop channel '{}': {}", name, e);
        } else {
            info!("Channel '{}' stopped", name);
        }
    }

    // 4. ACP shutdown.
    if let Err(e) = state.agents.acp.shutdown().await {
        warn!("Failed to shut down ACP control plane: {}", e);
    } else {
        info!("ACP control plane shut down");
    }

    // 5. Cron scheduler.
    if let Some(cron_arc) = state.scheduler.cron_scheduler.read().await.clone() {
        let mut scheduler = cron_arc.lock().await;
        if let Err(e) = scheduler.shutdown().await {
            warn!("Failed to shutdown cron scheduler: {}", e);
        } else {
            info!("Cron scheduler stopped");
        }
    }

    // 6. Dream scheduler.
    if let Some(mut scheduler) = state.memory.dream_scheduler.write().await.take() {
        scheduler.stop().await;
        if let Some(handle) = state
            .task_registry
            .remove_join_or_abort("dream_scheduler")
            .await
        {
            match timeout(Duration::from_secs(5), handle).await {
                Ok(_) => info!("Dream scheduler stopped"),
                Err(_) => warn!("Dream scheduler did not stop within timeout"),
            }
        } else {
            info!("Dream scheduler stopped");
        }
    }

    // 7. Standing orders manager.
    if let Some(mut manager) = state.memory.standing_order_manager.write().await.take() {
        manager.stop().await;
        info!("Standing orders manager stopped");
    }

    // 8. Disconnect MCP servers.
    let mcp_servers = state.tools.mcp_manager.list_servers().await;
    for server_id in mcp_servers {
        if let Err(e) = state.tools.mcp_manager.disconnect(&server_id).await {
            warn!("Failed to disconnect MCP server '{}': {}", server_id, e);
        }
    }

    // 9. Hot reload.
    if let Some(hot_reload) = state.infra.hot_reload.read().await.clone() {
        if let Err(e) = hot_reload.stop().await {
            warn!("Failed to stop hot reload manager: {}", e);
        }
    }

    // 10. Task scheduler.
    if let Some(ts_arc) = state.scheduler.task_scheduler.read().await.clone() {
        let mut scheduler = ts_arc.lock().await;
        if let Err(e) = scheduler.stop().await {
            warn!("Failed to stop task scheduler: {}", e);
        }
    }

    // 11. Browser bridge / pool.
    #[cfg(feature = "browser")]
    {
        let mut bridge_lock = state.infra.browser_bridge.write().await;
        if let Some(bridge) = bridge_lock.take() {
            bridge.shutdown().await;
            info!("Browser pool shut down");
        }
    }

    // 12. Abort remaining background tasks (includes followup timers now that they
    //     live in the unified registry).
    let background_handles = state.task_registry.take_all().await;
    for (_name, handle) in background_handles {
        handle.abort();
    }

    // 13. Plugin manager shutdown.
    if let Err(e) = state.infra.plugin_manager.shutdown().await {
        warn!("Failed to shutdown plugin manager: {}", e);
    }

    // 14. Storage is left to flush on process exit.
    info!("Gateway shutdown complete");
    Ok(())
}

// ── build_router ─────────────────────────────────────────────────────

/// Build the HTTP router.
pub(crate) async fn build_router(state: Arc<GatewayState>) -> Router {
    // Public tier: Webhooks (no authentication, signature verification per-channel)
    let public_router = super::webhooks::create_webhook_router(state.clone());

    // (Local OAuth login/logout was removed — the UI authenticates via the
    // Syscity Cloud OAuth flow instead; the auth_router is now empty.)

    // Admin tier: Essential APIs (not deprecated)
    let essential_public_router = Router::new()
        .route("/health", get(super::health_handler))
        .route("/ready", get(super::ready_handler))
        .route("/live", get(super::live_handler))
        .route("/metrics", get(super::metrics_handler))
        .route("/api/v1/artifacts/*path", get(super::artifact_handler));

    // Authenticated essential APIs (auth required)
    let essential_auth_router = Router::new()
        .route("/v1/chat/completions", post(super::openai_chat_completions_handler))
        .route("/v1/models", get(super::openai_list_models_handler))
        // (Everything else in the former admin tier — models, reload, channels,
        // device pairing, providers, plugins, cron, skills — is WS-only now,
        // driven by the admin WS methods in ws/admin_ws.rs.)
        .layer(from_fn_with_state(state.clone(), super::middleware::auth_middleware));

    let essential_router = essential_public_router.merge(essential_auth_router);

    // Apply remaining middleware layers to essential routes
    let admin_router = essential_router
        .layer(from_fn_with_state(state.clone(), super::middleware::rate_limit_middleware))
        .layer(from_fn_with_state(state.clone(), super::middleware::tailscale_auth_middleware))
        .layer(from_fn_with_state(
            state.clone(),
            super::middleware::trusted_proxy_auth_middleware,
        ))
        .layer(from_fn(super::middleware::security_headers_middleware))
        .with_state(state.clone());

    // WebSocket sub-router with mandatory auth validation middleware
    let ws_router = Router::new()
        .route("/ws", get(super::ws::ws_handler))
        .layer(from_fn_with_state(state.clone(), super::ws::ws_auth_middleware))
        .with_state(state.clone());

    // Build CORS layer from config
    let cors_layer = {
        let config = state.config.read().await;
        if config.security.cors.enabled {
            let mut cors = CorsLayer::new();
            if config.security.cors.allow_credentials {
                cors = cors.allow_credentials(true);
            }
            let has_wildcard = config
                .security
                .cors
                .allowed_origins
                .iter()
                .any(|o| o == "*");
            if has_wildcard && config.security.cors.allow_credentials {
                cors = cors.allow_origin(tower_http::cors::AllowOrigin::mirror_request());
            } else {
                for origin in &config.security.cors.allowed_origins {
                    if origin == "*" {
                        cors = cors.allow_origin(tower_http::cors::Any);
                    } else if let Ok(header_value) = origin.parse() {
                        cors = cors.allow_origin([header_value]);
                    }
                }
            }
            let methods: Vec<_> = config
                .security
                .cors
                .allowed_methods
                .iter()
                .filter_map(|m| m.parse().ok())
                .collect();
            if !methods.is_empty() {
                cors = cors.allow_methods(methods);
            }
            let headers: Vec<_> = config
                .security
                .cors
                .allowed_headers
                .iter()
                .filter_map(|h| h.parse().ok())
                .collect();
            if !headers.is_empty() {
                cors = cors.allow_headers(headers);
            }
            cors.max_age(std::time::Duration::from_secs(config.security.cors.max_age_secs as u64))
        } else {
            CorsLayer::new()
        }
    };

    // SPA frontend routes (serve built React app from embedded assets)
    let frontend_router = Router::new()
        .route("/", get(super::web_terminal_html_handler))
        .route("/favicon.ico", get(super::favicon_handler))
        // Cloud OAuth return URL (default `cloud.redirect_base`): serves the
        // SPA, whose App.tsx reads `#token=` here and persists it over WS
        // (`cloud.token`). Asset URLs are rewritten to absolute because the
        // route is nested (see cloud_login_callback_html_handler).
        .route(
            "/cloud/login/callback",
            get(super::cloud_login_callback_html_handler),
        )
        .route("/syscity.png", get(super::syscity_png_handler))
        .route("/manifest.webmanifest", get(super::manifest_handler))
        .route("/registerSW.js", get(super::register_sw_handler))
        .route("/sw.js", get(super::sw_js_handler))
        .route("/assets/*path", get(super::asset_handler));

    // Cloud session endpoints (feature `cloud`). Public tier: these are how
    // the web SPA logs into Syscity Cloud and persists the session token.
    #[cfg(feature = "cloud")]
    let cloud_router = Router::new()
        .route("/api/v1/login", get(super::handlers::cloud::login_handler))
        .with_state(state.clone());

    // Public engine status is now exposed via the WS `status.get` method
    // (ws/admin_ws.rs) — the REST endpoint was removed.

    // Merge all routers and apply global CORS
    let app = frontend_router
        .merge(public_router)
        .merge(admin_router)
        .merge(ws_router);

    #[cfg(feature = "cloud")]
    let app = app.merge(cloud_router);

    app.layer(cors_layer)
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Spawn an agent and track its task handle.
async fn spawn_agent_in_lifecycle(
    state: Arc<GatewayState>,
    id: String,
    config: AgentConfig,
) -> crate::Result<()> {
    spawn_agent_inner(state, id, config).await?;
    Ok(())
}

/// Run the quality gate check during startup.
///
/// Creates a temporary eval agent, runs all configured suites, and evaluates
/// criteria. If the gate fails, the error message includes details of which
/// criteria did not pass. Gate `shutdown_on_failure` determines whether this
/// error is fatal (blocking startup) or just a warning.
async fn run_quality_gate_check(
    state: Arc<GatewayState>,
    config: &GatewayConfig,
) -> crate::Result<()> {
    info!("═══ Quality Gate: {} ═══", config.quality_gate.name);

    // 1. Resolve provider from config
    let provider_type = config.model_provider.clone();
    let api_key = match config.providers.get(&provider_type) {
        Some(p) => {
            let key = p.effective_key().await;
            if key.is_empty() {
                None
            } else {
                Some(key)
            }
        }
        None => None,
    }
    .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
    .or_else(|| std::env::var("OPENAI_API_KEY").ok());

    let base_url = config
        .providers
        .get(&provider_type)
        .and_then(|p| p.base_url.clone())
        .or_else(|| std::env::var("SYSCITY_BASE_URL").ok());

    let model = Some(config.model.clone());

    let provider = crate::providers::resolver::resolve_provider(
        &provider_type,
        api_key,
        base_url,
        model.clone(),
        None,
    )
    .map_err(|e| {
        crate::error::SyscityError::Validation(format!(
            "Quality gate: failed to create provider '{}': {}",
            provider_type, e
        ))
    })?;

    // 2. Create eval tool registry
    let tool_registry = Arc::new(create_eval_tool_registry(None));

    // 3. Create a temporary agent for eval
    let agent_config = config.default_agent.clone();
    let agent =
        Arc::new(crate::agent::Agent::new(agent_config, provider.clone(), tool_registry.clone()));

    // 4. Create critic (if needed for criteria)
    let mut critic = crate::agent::reflection::critic::Critic::new(provider);
    if let Some(ref model_name) = model {
        critic = critic.with_model(model_name.clone());
    }

    // 5. Build harness and gate.
    // The harness carries a LayeredScorer (§三 人工复核) so every trial routed
    // through the daemon gate persists a human-review case on low-confidence /
    // conflict signals, honoring `eval.human_review.sampling_rate`. The critic
    // is consumed by the harness; the scorer only calls `route_scored`, so it
    // is built with no internal critic.
    let evals_dir = crate::eval::loader::default_evals_dir();
    let harness = crate::eval::harness::EvalHarness::new(agent.clone(), Some(critic)).with_scorer(
        crate::eval::LayeredScorer::new(None, crate::eval::ScorerConfig::default())
            .with_review_store(crate::eval::HumanReviewStore::new(&evals_dir))
            .with_sampling_rate(config.eval.human_review.sampling_rate),
    );

    let gate = match crate::gateway::quality_gate::QualityGate::from_config(
        &config.quality_gate,
        harness,
        evals_dir,
    ) {
        Some(g) => g,
        None => {
            info!("Quality gate not configured — skipping");
            return Ok(());
        }
    };
    let gate = gate.with_badcase_governance(crate::eval::recycle::BadcaseGovernance::from_config(
        &config.eval.badcase_governance,
    ));
    // Attach the runtime stores backing the compression gate and the shadow
    // gate's online replay (§十二 ⑧ / §09). `None` stores leave them inert.
    let gate = gate
        .with_stores(state.infra.pending_badcase_store.clone(), state.infra.sample_store.clone());

    // 6. Run the gate (returns result + release decision)
    let (result, decision) = gate.check().await;

    // 7. Print results
    let decision_label = match &decision {
        crate::gateway::quality_gate::ReleaseDecision::Proceed => "PROCEED",
        crate::gateway::quality_gate::ReleaseDecision::Rollback => "ROLLBACK",
        crate::gateway::quality_gate::ReleaseDecision::Degrade => "DEGRADE",
    };
    info!("{}", result);
    info!("Release decision: {}", decision_label);

    // 8. Cleanup
    agent.shutdown().await?;

    match decision {
        crate::gateway::quality_gate::ReleaseDecision::Proceed => {
            info!("✅ Quality gate passed — proceeding with startup");
            Ok(())
        }
        crate::gateway::quality_gate::ReleaseDecision::Rollback => {
            let err = crate::error::SyscityError::Validation(
                "Quality gate ROLLBACK — blocking startup".into(),
            );
            if config.quality_gate.shutdown_on_failure {
                Err(err)
            } else {
                warn!("{} (shutdown_on_failure disabled, continuing)", err);
                Ok(())
            }
        }
        crate::gateway::quality_gate::ReleaseDecision::Degrade => {
            warn!("⚠️ Quality gate DEGRADE — starting in degraded mode");
            Ok(())
        }
    }
}

/// Create a minimal tool registry for quality gate eval.
fn create_eval_tool_registry(
    acp: Option<Arc<crate::acp::AcpControlPlane>>,
) -> crate::tools::ToolRegistry {
    let mut registry = crate::tools::ToolRegistry::new();
    registry.register(Box::new(crate::tools::shell::ShellTool::new()));
    registry.register(Box::new(crate::tools::file::FileReadTool::new()));
    registry.register(Box::new(crate::tools::file::FileWriteTool::new()));
    registry.register(Box::new(crate::tools::file::FileEditTool::new()));
    registry.register(Box::new(crate::tools::grep::GrepTool::new()));
    registry.register(Box::new(crate::tools::file::GlobTool::new()));
    registry.register(Box::new(crate::tools::web::WebSearchTool::new()));
    registry.register(Box::new(crate::tools::web::WebFetchTool::new()));
    registry.register(Box::new(crate::tools::todo_tool::TodoTool::new()));
    registry.register(Box::new(crate::tools::time::TimeTool::new()));
    if let Some(acp) = acp {
        registry.register(Box::new(crate::tools::AcpSpawnTool::new(acp.clone(), None)));
        registry.register(Box::new(crate::tools::AcpSessionTool::new(acp.clone())));
        registry.register(Box::new(crate::tools::SessionsSendTool::new(acp)));
    }
    registry
}

/// Register the discovered tools of a connected MCP server into the agent
/// tool registry (`mcp__{server_id}__{tool}`). Shared by the boot-time
/// auto-connect and runtime add/connect paths so tools become available to
/// agents immediately.
pub(crate) async fn register_mcp_tools(
    state: &Arc<GatewayState>,
    server_id: &str,
    tools: &[crate::mcp::McpToolDefinition],
    max_tools_config: usize,
) {
    let max_tools = if max_tools_config == 0 {
        tools.len()
    } else {
        max_tools_config.min(tools.len())
    };

    if let Some(client_arc) = state.tools.mcp_manager.get_client(server_id).await {
        for tool in tools.iter().take(max_tools) {
            let wrapper = Arc::new(McpToolWrapper::new(client_arc.clone(), server_id, tool));
            state.tools.registry.register_dynamic(wrapper);
            debug!("  Registered MCP tool: mcp__{}__{}", server_id, tool.name);
        }
    }
}

/// Auto-connect MCP servers from config and register their tools.
///
/// Runs all connections concurrently in the background so they never block
/// the HTTP listener from starting.
pub(crate) fn init_mcp_servers(state: Arc<GatewayState>, config: &GatewayConfig) {
    let servers = &config.mcp.servers;
    if servers.is_empty() {
        debug!("No MCP servers configured");
        return;
    }

    info!("Auto-connecting {} configured MCP server(s) (background)…", servers.len());

    for (server_id, server_config) in servers {
        if !server_config.auto_connect {
            info!("MCP server '{}' has auto_connect=false, skipping", server_id);
            continue;
        }

        let bg_state = state.clone();
        let sid = server_id.clone();
        let cfg = server_config.clone();
        tokio::spawn(async move {
            match bg_state.tools.mcp_manager.connect(&sid, cfg.clone()).await {
                Ok(tools) => {
                    info!("✅ MCP server '{}' connected: {} tool(s) discovered", sid, tools.len());
                    register_mcp_tools(&bg_state, &sid, &tools, cfg.max_tools).await;
                }
                Err(e) => {
                    warn!("Failed to connect MCP server '{}': {}", sid, e);
                }
            }
        });
    }
}
