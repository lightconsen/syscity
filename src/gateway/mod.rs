//! Gateway Control Plane
//!
//! The Gateway is the control plane for Syscity, managing:
//! - Multi-channel message routing (WhatsApp, Telegram, Feishu, etc.)
//! - Session management and routing to agents
//! - Agent spawning and lifecycle management
//! - WebSocket/HTTP API for channel adapters
//! - Authentication and security policies
// INVARIANTS-NONE: boot wiring only; runtime guarantees are owned by the subsystems it wires (tasks go through TaskRegistry).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::channels::ChannelAcpBridge;
use crate::hooks::ShellHookBridge;

pub mod auth;
pub mod command_provider;
pub mod commands;
pub mod config;
pub mod handlers;
pub mod hooks;
pub mod init;
pub mod middleware;
pub mod protocol;
pub mod quality_gate;
pub mod shadow_replay;
pub use quality_gate::{
    ABReport, FeedbackCollector, PhaseStore, ProdTurn, ReleaseDecision, ReleaseSignals,
    ShadowReport,
};
pub mod rate_limit;
pub mod send_policy;
pub mod state;
pub use config::*;
pub use state::*;

pub mod task_registry;
pub use task_registry::*;

pub mod acp_ext;
pub use acp_ext::*;
pub mod agent_spawn;
pub mod apply_config;
pub(crate) use agent_spawn::{create_default_tool_registry, spawn_agent_inner};
pub mod dispatch;
pub mod feedback;
pub use feedback::*;
pub(crate) mod goal_spawn;
pub mod hot_reload;
pub mod lifecycle;
pub mod runtime;
pub use runtime::*;
pub mod types;
pub use types::*;
pub mod watchdog;
pub use watchdog::{RepairRecord, RepairState};
pub mod webhooks;
pub mod ws;
use handlers::*;

// Configuration types live in `gateway::config` (re-exported above).

/// Derive the runtime config for the built-in `default` agent from the
/// persisted `default_agent` config by appending the agent-identity block to
/// its system prompt. Used both at startup spawn and when pushing live
/// `default_agent.*` updates so the running agent keeps its identity context.
pub(crate) fn augment_default_agent_config(
    base: &crate::agent::AgentConfig,
) -> crate::agent::AgentConfig {
    let mut config = base.clone();
    let default_agent_dir = crate::dirs::agents_dir().join("default");
    config.system_prompt = format!(
        "{}\n\n## Agent Identity\n\nYour agent ID is: `default`\nYour agent directory is: \
         `{}`\nYou may edit files in your agent directory (including HEARTBEAT.md) to manage your \
         personality and periodic tasks when explicitly asked by the user.",
        config.system_prompt,
        default_agent_dir.display()
    );
    config
}

impl GatewayState {
    /// Centralized access check for incoming messages.
    ///
    /// Returns `Ok(())` if the message is allowed, or `Err(reason)` if it
    /// should be dropped.
    pub async fn check_incoming_access(
        &self,
        channel: &str,
        user_id: &str,
        content: &str,
        mention: &crate::channels::MentionState,
    ) -> Result<(), String> {
        use crate::security::runtime_audit::AuditEventType;

        let channel_config = {
            let config = self.config.read().await;
            config.channels.get(channel).cloned()
        };

        if let Some(ref ch_cfg) = channel_config {
            // 1. Blocklist check
            if ch_cfg.is_blocked(user_id) {
                let reason = format!("User {} is blocked on channel {}", user_id, channel);
                self.auth
                    .audit_log
                    .log(AuditEventType::AccessCheck, user_id, channel, false, &reason, None)
                    .await;
                return Err(reason);
            }

            // 2. DM Policy check
            use crate::security::pairing::DmPolicy;
            match ch_cfg.dm_policy {
                DmPolicy::Open => {}
                DmPolicy::Pairing => {
                    if !self
                        .auth
                        .pairing_store
                        .is_authorized(channel, user_id)
                        .await
                    {
                        // Create pairing request silently and drop message
                        let _ = self
                            .auth
                            .pairing_store
                            .request_access(channel, user_id, None)
                            .await;
                        let reason = format!(
                            "User {} not authorized on channel {} (pairing required)",
                            user_id, channel
                        );
                        self.auth
                            .audit_log
                            .log(
                                AuditEventType::PairingRequest,
                                user_id,
                                channel,
                                false,
                                &reason,
                                None,
                            )
                            .await;
                        return Err(reason);
                    }
                }
                DmPolicy::Allowlist => {
                    if !ch_cfg.is_in_allowlist(user_id)
                        && !self
                            .auth
                            .pairing_store
                            .is_authorized(channel, user_id)
                            .await
                    {
                        let reason =
                            format!("User {} not in allowlist for channel {}", user_id, channel);
                        self.auth
                            .audit_log
                            .log(
                                AuditEventType::AccessCheck,
                                user_id,
                                channel,
                                false,
                                &reason,
                                None,
                            )
                            .await;
                        return Err(reason);
                    }
                }
            }

            // 3. Mention gating (require_mention + MentionGate)
            if !mention.should_process(ch_cfg.require_mention) {
                let reason = format!(
                    "Message from {} on channel {} ignored (mention required in groups)",
                    user_id, channel
                );
                self.auth
                    .audit_log
                    .log(AuditEventType::AccessCheck, user_id, channel, false, &reason, None)
                    .await;
                return Err(reason);
            }

            // 3b. MentionGate policy check
            if matches!(mention, crate::channels::MentionState::Mentioned) {
                let mention_allowed = self.auth.mention_gate.check(channel, "*").await;
                if !mention_allowed {
                    let reason = format!(
                        "Mention gate blocked message on channel {} (policy: {})",
                        channel,
                        self.auth.mention_gate.policy().await
                    );
                    self.auth
                        .audit_log
                        .log(AuditEventType::AccessCheck, user_id, channel, false, &reason, None)
                        .await;
                    return Err(reason);
                }
            }
        }

        // 4. Command gate check
        let decision = self.auth.command_gate.check(user_id, content);
        if !decision.is_allowed() {
            let reason = match decision {
                crate::tools::command_gate::AccessDecision::Denied { reason, .. } => reason,
                _ => "Unknown denial reason".to_string(),
            };
            let msg = format!("Command gate denied for user {}: {}", user_id, reason);
            self.auth
                .audit_log
                .log(
                    AuditEventType::CommandGate,
                    user_id,
                    channel,
                    false,
                    &msg,
                    Some(serde_json::json!({"reason": reason})),
                )
                .await;
            return Err(msg);
        }

        // Log successful access
        self.auth
            .audit_log
            .log(AuditEventType::AccessCheck, user_id, channel, true, "Access allowed", None)
            .await;

        Ok(())
    }
}

// Runtime types (`BufferedMessage`, `AgentHandle`, `AgentCommand`,
// `AgentQuery`, `GatewayEvent`, `AgentStatus`) live in `gateway::runtime` and
// are re-exported via `pub use runtime::*;` above.

// Repair tracking (`RepairRecord`, `RepairState`) and the watchdog/repair
// loops moved to `gateway::watchdog`. WS/REST request DTOs (`WsQuery`,
// `SwitchModelRequest`, `SendMessageRequest`, etc.) live in `gateway::types`
// and are re-exported via `pub use types::*;` above.

/// Gateway control plane
pub struct Gateway {
    pub(crate) state: Arc<GatewayState>,
    pub(crate) shutdown_token: CancellationToken,
}

/// Optional wiring supplied at construction time (mobile).
///
/// All fields default to `None`; desktop builds never set them, so the
/// desktop code path is byte-for-byte unchanged.
#[derive(Default)]
pub struct GatewayOptions {
    /// Native device bridge (camera/geolocation/notifications/SAF/adb).
    ///
    /// Only constructed on mobile; `None` on desktop keeps every `device_*`
    /// tool and `device.*` WS method unavailable.
    pub device_bridge: Option<Arc<dyn crate::device::DeviceBridge>>,
    /// Explicit path to the CC-compatible `hooks.json` shell-hooks file.
    ///
    /// When `None`, the bridge falls back to a `hooks.json` sibling of the
    /// config file, then to `~/.syscity/hooks.json`.
    pub hooks_file: Option<PathBuf>,
}

impl Gateway {
    /// Create a new gateway instance (desktop default wiring).
    pub async fn new(config: GatewayConfig, config_path: Option<PathBuf>) -> crate::Result<Self> {
        Self::with_options(config, config_path, GatewayOptions::default()).await
    }

    /// Create a new gateway instance with explicit mobile wiring.
    pub async fn with_options(
        mut config: GatewayConfig,
        config_path: Option<PathBuf>,
        options: GatewayOptions,
    ) -> crate::Result<Self> {
        let device_bridge = options.device_bridge;
        let (event_tx, _) = broadcast::channel(1000);
        let (log_tx, _) = broadcast::channel(1000);
        let (inbound_entry_tx, inbound_entry_rx) =
            mpsc::channel::<crate::channels::IncomingMessage>(1000);
        let (routed_tx, routed_rx) = mpsc::channel(1000);
        let shutdown_token = CancellationToken::new();

        // One-time migration of the legacy ~/.syscity/mcp_env store into
        // ~/.syscity/secrets/mcp-env (idempotent; no-op when absent), plus a
        // sweep of any old mcp_tokens sidecars still carrying plaintext token
        // fields (design §8.6).
        if let Err(e) = crate::secrets::migrate_legacy_mcp_env().await {
            warn!("Legacy mcp_env migration failed: {}", e);
        }
        if let Err(e) = crate::mcp::migrate_legacy_mcp_tokens().await {
            warn!("Legacy mcp_tokens migration failed: {}", e);
        }

        let storage_init = init::storage::init_storage(&config).await?;
        let storage = storage_init.storage;
        let unified_vector_store = storage_init.unified_vector_store;
        let sqlite_pool = storage_init.sqlite_pool;
        let session_store = storage_init.session_store;
        let auth_store = storage_init.auth_store;
        let feedback_store = storage_init.feedback_store;
        let pending_badcase_store = storage_init.pending_badcase_store;
        let decision_trace_store = storage_init.decision_trace_store;
        let sample_store = storage_init.sample_store;
        let audit_log = storage_init.audit_log;
        let audit_log_dyn = storage_init.audit_log_dyn;

        // Shell hooks bridge: loaded once at startup, read from the
        // explicit option, a `hooks.json` sibling of the config file, or the
        // default `~/.syscity/hooks.json`. Never fails — a missing/broken
        // file yields an empty bridge and the daemon boots normally.
        let hooks_path = options
            .hooks_file
            .clone()
            .or_else(|| config_path.clone().map(|p| p.with_file_name("hooks.json")))
            .unwrap_or_else(|| crate::dirs::config_dir().join("hooks.json"));
        let shell_hooks = ShellHookBridge::load(&hooks_path, Some(audit_log_dyn.clone()));

        // Migrate legacy (alias-era) provider/model config in place before the
        // router is built, persisting once if anything changed so disk, the
        // live router, and the frontend agree from the first boot.
        let migrated = init::agents::migrate_model_router_config(&mut config).await;
        if migrated {
            if let Some(config_path) = config_path.clone() {
                if let Err(e) = handlers::config::persist_config_atomic(&config, &config_path).await
                {
                    warn!("Failed to persist migrated config: {}", e);
                }
            }
        }

        let acp = init::agents::init_acp(&config, session_store.clone()).await;

        let task_registry = Arc::new(crate::gateway::task_registry::TaskRegistry::new());
        let model_router =
            init::agents::init_model_router(&config, task_registry.clone(), shutdown_token.clone())
                .await;

        let (skills_manager, agent_registry, session_manager) =
            init::agents::init_agent_state().await?;

        let tools_init = init::tools::init_tools(
            &config,
            init::tools::ToolSystemDeps {
                acp: acp.clone(),
                session_store: session_store.clone(),
                audit_log_dyn: audit_log_dyn.clone(),
                model_router: model_router.clone(),
                task_registry: task_registry.clone(),
                device_bridge: device_bridge.clone(),
                skills_manager: skills_manager.clone(),
                shell_hooks: shell_hooks.clone(),
            },
        )
        .await?;

        init::agents::configure_acp_agent_builder(
            &acp,
            &config,
            model_router.clone(),
            tools_init.tool_registry.clone(),
            skills_manager.clone(),
        )
        .await;

        let security_init =
            init::security::init_security(&config, audit_log_dyn.clone(), auth_store.clone())
                .await?;

        let pipelines_init = init::pipelines::init_pipelines(
            &config,
            sqlite_pool.as_ref(),
            model_router.clone(),
            routed_tx.clone(),
        )
        .await?;

        // Restore previously paired devices and keep new pairings durable.
        let device_pairing_store = {
            let store = crate::security::device_pairing::DevicePairingStore::new();
            match auth_store.as_ref() {
                Some(auth) => store.with_persistence(auth.clone()).await?,
                None => store,
            }
        };

        let state = Arc::new(GatewayState {
            config: Arc::new(RwLock::new(Arc::new(config.clone()))),
            start_time: Instant::now(),
            config_path: config_path.clone(),
            mcps_path: Some(crate::dirs::config_dir().join("mcp.toml")),
            task_registry: task_registry.clone(),
            shutdown_token: shutdown_token.clone(),
            auth: AuthState {
                manager: security_init.auth_manager.clone(),
                pairing_store: Arc::new(crate::security::pairing::PairingStore::new()),
                device_pairing_store: Arc::new(device_pairing_store),
                tailscale_authenticator: {
                    let ttl = config.security.tailscale_auth_ttl_secs;
                    Some(Arc::new(crate::security::tailscale::TailscaleAuthenticator::new(
                        std::time::Duration::from_secs(ttl),
                    )))
                },
                trusted_proxy_authenticator: {
                    let tp_config = config.security.trusted_proxy.clone();
                    if tp_config.enabled {
                        Some(Arc::new(
                            crate::security::trusted_proxy::TrustedProxyAuthenticator::new(
                                tp_config,
                            ),
                        ))
                    } else {
                        None
                    }
                },
                rate_limiter: security_init.rate_limiter.clone(),
                multi_tier_rate_limiter: security_init.multi_tier_rate_limiter.clone(),
                audit_log: audit_log.clone(),
                command_gate: security_init.command_gate.clone(),
                mention_gate: security_init.mention_gate.clone(),
            },
            agents: AgentState {
                agents: Arc::new(RwLock::new(HashMap::new())),
                pending_spawns: Arc::new(std::sync::Mutex::new(HashSet::new())),
                router: pipelines_init.agent_router.clone(),
                registry: agent_registry.clone(),
                manager: session_manager.clone(),
                group_manager: Arc::new(RwLock::new(crate::agent::GroupSessionManager::new())),
                store: session_store.clone(),
                message_buffer: Arc::new(RwLock::new(HashMap::new())),
                route_resolver: Arc::new(crate::agent::RouteResolver::new("default")),
                cost_guard: crate::agent::CostGuard::new(
                    config.cost_guard.daily_limit_cents,
                    config.cost_guard.hourly_action_limit,
                ),
                repair_state: Arc::new(RepairState::new()),
                acp: acp.clone(),
                goal_cancellers: Arc::new(RwLock::new(HashMap::new())),
            },
            channels: ChannelState {
                channels: tools_init.channels.clone(),
                extensions: tools_init.channel_extensions.clone(),
                reply_dispatcher: pipelines_init.reply_dispatcher.clone(),
                snapshot_store: None,
                health_monitor: None,
                acp_bridge: Some(Arc::new(ChannelAcpBridge::new(acp.command_tx()))),
                session_channels: Arc::new(RwLock::new(HashMap::new())),
                webhook_sessions: Arc::new(RwLock::new(HashMap::new())),
            },
            memory: MemoryState {
                vector: RwLock::new(None),
                session_search: RwLock::new(None),
                manager: tools_init.memory_manager_holder.clone(),
                dream_scheduler: RwLock::new(None),
                dream_metrics: Arc::new(crate::memory::DreamMetrics::default()),
                standing_order_manager: RwLock::new(None),
                kb_manager: RwLock::new(None),
            },
            tools: ToolState {
                registry: tools_init.tool_registry.clone(),
                mcp_manager: tools_init.mcp_manager.clone(),
                connector_manager: tools_init.connector_manager.clone(),
                approval_queue: tools_init.approval_queue.clone(),
                ask_queue: tools_init.ask_queue.clone(),
                skills_manager: skills_manager.clone(),
                canvas_manager: tools_init.canvas_manager.clone(),
                computer_adapter: Arc::new(tokio::sync::RwLock::new(
                    tools_init.computer_adapter.clone(),
                )),
                planner_handle: tools_init.planner_handle.clone(),
            },
            pipelines: PipelineState {
                inbound: pipelines_init.inbound_pipeline.clone(),
                outbound: pipelines_init.outbound_pipeline.clone(),
                side_effect_executor: pipelines_init.side_effect_executor.clone(),
                sse_streamer: pipelines_init.sse_streamer.clone(),
                routed_tx: routed_tx.clone(),
                inbound_entry: inbound_entry_tx.clone(),
            },
            events: EventState {
                tx: event_tx.clone(),
                log_tx: log_tx.clone(),
                hook_registry: Arc::new(
                    hooks::EventHookRegistry::new().with_task_registry(task_registry.clone()),
                ),
            },
            infra: InfraState {
                storage: storage.clone(),
                runtime_settings: Arc::new(RwLock::new(HashMap::new())),
                transcript_store: {
                    let store = crate::agent::TranscriptStore::new(crate::dirs::transcripts_dir());
                    Arc::new(store)
                },
                artifact_store: {
                    let store = crate::agent::ArtifactStore::new(crate::dirs::artifacts_dir());
                    Arc::new(store)
                },
                disk_budget: {
                    let manager = crate::agent::DiskBudgetManager::new(crate::dirs::budget_dir());
                    if let Err(e) = manager.init() {
                        warn!("Failed to initialize disk budget manager: {}", e);
                    }
                    Arc::new(manager)
                },
                session_file_manager: {
                    let manager =
                        crate::agent::SessionFileManager::new(crate::dirs::session_files_dir());
                    if let Err(e) = manager.init().await {
                        warn!("Failed to initialize session file manager: {}", e);
                    }
                    Arc::new(manager)
                },
                hot_reload: RwLock::new(None),
                plugin_manager: tools_init.plugin_manager.clone(),
                model_router: model_router.clone(),
                shell_hooks: shell_hooks.clone(),
                feedback_store: feedback_store.clone(),
                pending_badcase_store: pending_badcase_store.clone(),
                decision_trace_store: decision_trace_store.clone(),
                sample_store: sample_store.clone(),
                optimizer: Arc::new(crate::eval::OptimizerRuntime::default()),
                #[cfg(feature = "browser")]
                browser_bridge: tokio::sync::RwLock::new(None),
            },
            sdk: SdkState {
                provider_sdk: Arc::new(RwLock::new(crate::providers::ProviderSdk::new())),
                tool_sdk: Arc::new(RwLock::new(crate::tools::ToolSdk::new())),
            },
            scheduler: SchedulerState {
                task_scheduler: RwLock::new(None),
                heartbeat_wake_tx: RwLock::new(None),
                heartbeat_event_tx: RwLock::new(None),
                cron_scheduler: RwLock::new(None),
            },
            device: DeviceState {
                bridge: RwLock::new(device_bridge),
            },
            update: UpdateState::new(),
            embedded: std::env::var("SYSCITY_EMBEDDED").is_ok(),
        });

        if let Some(ref store) = state.agents.store {
            let mut mgr = state.agents.manager.write().await;
            mgr.with_store(store.clone());
        }

        state.agents.acp.set_event_tx(state.events.tx.clone()).await;

        {
            let event_tx = state.events.tx.clone();
            let mut mcp_event_rx = tools_init.mcp_event_rx;
            let shutdown_token = shutdown_token.clone();
            let mcp_forward_handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(event) = mcp_event_rx.recv() => {
                            let gateway_event = match event {
                                crate::mcp::McpEvent::Connected {
                                    server_id, tools, prompts, resources,
                                } => GatewayEvent::McpConnected {
                                    server_id, tools, prompts, resources,
                                },
                                crate::mcp::McpEvent::Disconnected { server_id, reason } => {
                                    GatewayEvent::McpDisconnected { server_id, reason }
                                }
                                crate::mcp::McpEvent::Recovered { server_id, attempt } => {
                                    GatewayEvent::McpRecovered { server_id, attempt }
                                }
                                crate::mcp::McpEvent::ResourceChanged { server_id, uri } => {
                                    GatewayEvent::McpResourceChanged { server_id, uri }
                                }
                                crate::mcp::McpEvent::AuthRequired { server_id, auth_url } => {
                                    GatewayEvent::McpAuthRequired { server_id, auth_url }
                                }
                                crate::mcp::McpEvent::AuthComplete { server_id } => {
                                    GatewayEvent::McpAuthComplete { server_id }
                                }
                                crate::mcp::McpEvent::AuthFailed { server_id, reason } => {
                                    GatewayEvent::McpAuthFailed { server_id, reason }
                                }
                                crate::mcp::McpEvent::TokenRefreshed { server_id } => {
                                    GatewayEvent::McpTokenRefreshed { server_id }
                                }
                            };
                            if let Err(e) = event_tx.send(gateway_event) {
                                debug!("No receivers for MCP event: {}", e);
                            }
                        }
                        _ = shutdown_token.cancelled() => {
                            info!("MCP forward task received shutdown signal, exiting");
                            break;
                        }
                    }
                }
            });
            state
                .task_registry
                .insert_join("mcp_forward", mcp_forward_handle)
                .await;
        }

        // One-shot observability retention sweep at startup, then idle until
        // shutdown (`observe.retention_days`; 0 disables auto-cleanup).
        if config.observe.retention_days > 0 {
            let store = session_store.clone();
            let days = config.observe.retention_days;
            let shutdown_token = shutdown_token.clone();
            let retention_handle = tokio::spawn(async move {
                let cutoff = crate::observe::prune::cutoff_date(days);
                let (dirs, files) =
                    crate::observe::prune::prune_turn_dirs(&crate::dirs::turns_dir(), &cutoff);
                let db_rows = match store.as_ref() {
                    Some(s) => match s
                        .delete_metrics_before(crate::observe::prune::cutoff_ms(days))
                        .await
                    {
                        Ok((l, t, o, sn)) => Some(l + t + o + sn),
                        Err(e) => {
                            warn!("Observability retention sweep failed (DB): {}", e);
                            None
                        }
                    },
                    None => None,
                };
                info!(
                    "Observability retention: pruned {} dirs / {} files{} ({} days)",
                    dirs,
                    files,
                    match db_rows {
                        Some(rows) => format!(", {} DB rows", rows),
                        None => String::new(),
                    },
                    days
                );
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        info!("Observability retention task received shutdown signal, exiting");
                    }
                }
            });
            state
                .task_registry
                .insert_join("observe_retention", retention_handle)
                .await;
        }

        if let Err(e) = state.auth.audit_log.init().await {
            warn!("Failed to initialize persistent audit log: {}", e);
        }

        state
            .tools
            .registry
            .register_dynamic(Arc::new(crate::tools::AgentsListTool::new(
                state.agents.registry.clone(),
            )));
        state
            .tools
            .registry
            .register_dynamic(Arc::new(crate::tools::GatewayTool::new(state.clone())));
        state
            .tools
            .registry
            .register_dynamic(Arc::new(crate::tools::MessageTool::new(state.clone())));
        state
            .tools
            .registry
            .register_dynamic(Arc::new(crate::tools::CanvasTool::new(
                state.tools.canvas_manager.clone(),
            )));

        {
            let mut provider_sdk = state.sdk.provider_sdk.write().await;
            provider_sdk
                .sync_from_model_router(&state.infra.model_router)
                .await;
        }
        {
            let mut tool_sdk = state.sdk.tool_sdk.write().await;
            tool_sdk.sync_from_tool_registry(&state.tools.registry);
        }

        init::services::init_late_services(
            &config,
            &state,
            sqlite_pool.as_ref(),
            unified_vector_store,
        )
        .await?;

        let inbound_handle = tokio::spawn(dispatch::process_inbound_entries(
            state.clone(),
            inbound_entry_rx,
            shutdown_token.clone(),
        ));
        state
            .task_registry
            .insert_join("message:inbound", inbound_handle)
            .await;
        let routed_handle = tokio::spawn(dispatch::process_routed_messages(
            state.clone(),
            routed_rx,
            shutdown_token.clone(),
        ));
        state
            .task_registry
            .insert_join("message:routed", routed_handle)
            .await;

        Ok(Self { state, shutdown_token })
    }

    /// Return a clone of the internal `ModelRouter` arc.
    pub fn model_router(&self) -> Arc<crate::model_router::ModelRouter> {
        self.state.infra.model_router.clone()
    }

    /// Return a clone of the internal `ToolRegistry` arc.
    pub fn tool_registry(&self) -> Arc<crate::tools::ToolRegistry> {
        self.state.tools.registry.clone()
    }

    /// Return a clone of the gateway shutdown token.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }

    /// Start the gateway
    pub async fn start(&self) -> crate::Result<()> {
        let config = { self.state.config.read().await.as_ref().clone() };
        lifecycle::start_gateway(self.state.clone(), config, self.shutdown_token.clone()).await
    }

    /// Gracefully shut down the gateway and its subsystems.
    pub async fn stop(&self) -> crate::Result<()> {
        lifecycle::stop_gateway(&self.shutdown_token, &self.state).await
    }

    /// Spawn a new agent
    async fn spawn_agent(
        &self,
        id: String,
        config: crate::agent::AgentConfig,
    ) -> crate::Result<()> {
        spawn_agent_inner(self.state.clone(), id, config).await?;
        Ok(())
    }

    /// Spawn an agent from its personality (on-demand spawning)
    ///
    /// Returns the agent handle and a boolean indicating whether the agent was
    /// newly spawned (`true`) or already existed (`false`). This keeps the
    /// spawn-and-lookup atomic from the caller's perspective and avoids races
    /// where a concurrent spawn succeeds but the caller cannot find the handle.
    pub async fn spawn_agent_from_personality(
        &self,
        agent_id: &str,
    ) -> crate::Result<(AgentHandle, bool)> {
        // Fast path: agent already exists.
        {
            let agents = self.state.agents.agents.read().await;
            if let Some(handle) = agents.get(agent_id) {
                return Ok((handle.clone(), false));
            }
        }

        // Get personality from registry
        let personality = {
            let registry = self.state.agents.registry.read().await;
            match registry.get(agent_id) {
                Some(p) => p.clone(),
                None => {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Agent '{}' not found in registry",
                        agent_id
                    )));
                }
            }
        };

        info!("🚀 On-demand spawning agent '{}' from personality", agent_id);

        // Convert personality to config
        let config = personality.to_agent_config();

        // Spawn the agent
        self.spawn_agent(agent_id.to_string(), config).await?;

        // The handle must now be present; return it directly.
        let agents = self.state.agents.agents.read().await;
        match agents.get(agent_id).cloned() {
            Some(handle) => Ok((handle, true)),
            None => Err(crate::error::SyscityError::Validation(format!(
                "Agent '{}' was spawned but disappeared immediately",
                agent_id
            ))),
        }
    }

    /// Spawn all discovered agents
    pub async fn spawn_all_discovered_agents(&self) -> crate::Result<usize> {
        let agent_ids: Vec<String> = {
            let registry = self.state.agents.registry.read().await;
            registry.list()
        };

        let mut spawned = 0;
        for agent_id in agent_ids {
            match self.spawn_agent_from_personality(&agent_id).await {
                Ok((_, true)) => {
                    info!("✅ Auto-spawned agent '{}'", agent_id);
                    spawned += 1;
                }
                Ok((_, false)) => {
                    debug!("Agent '{}' already spawned, skipping", agent_id);
                }
                Err(e) => {
                    warn!("Failed to spawn agent '{}': {}", agent_id, e);
                }
            }
        }

        info!("Auto-spawned {} agents from registry", spawned);
        Ok(spawned)
    }

    /// Register a channel extension with the gateway.
    ///
    /// Extensions are wired into the inbound/outbound pipelines and replace
    /// the ad-hoc per-channel initialisation code.
    /// Get or spawn agent by ID (on-demand)
    pub async fn get_or_spawn_agent(&self, agent_id: &str) -> crate::Result<Option<AgentHandle>> {
        match self.spawn_agent_from_personality(agent_id).await {
            Ok((handle, _)) => Ok(Some(handle)),
            Err(crate::error::SyscityError::Validation(_)) => {
                // Agent not found in personality registry is treated as "no agent".
                Ok(None)
            }
            Err(e) => {
                warn!("Failed to get or spawn agent '{}': {}", agent_id, e);
                Ok(None)
            }
        }
    }
}

/// Validate authentication configuration for ambiguity and conflicts.
///
/// Fails fast when the configured security settings cannot work at runtime.
pub(crate) fn validate_auth_config(config: &GatewayConfig) -> crate::Result<()> {
    if !config.security.enabled || !config.security.auth_required {
        return Ok(());
    }

    let has_token = config
        .security
        .shared_token
        .as_deref()
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    let mode = config.security.auth_mode;
    let mode_unset = mode == crate::gateway::protocol::AuthMode::None;

    // Token mode requires a non-empty shared token.
    if mode == crate::gateway::protocol::AuthMode::Token && !has_token {
        return Err(crate::error::SyscityError::Validation(
            "auth_mode is 'token' but shared_token is missing or empty".into(),
        ));
    }

    // When auth is required, at least one mechanism must be configured.
    let has_device = mode == crate::gateway::protocol::AuthMode::Device;
    let has_tailscale = mode == crate::gateway::protocol::AuthMode::Tailscale;
    if !has_token && !has_device && !has_tailscale {
        return Err(crate::error::SyscityError::Validation(
            "security.auth_required is true but no authentication mechanism is configured \
             (shared_token, device, or tailscale)"
                .into(),
        ));
    }

    // Warn when token is configured but auth_mode is not Token
    if has_token && mode_unset {
        tracing::warn!(
            "shared_token is configured but auth_mode is 'none'. Set auth_mode to 'token' for \
             consistent authentication."
        );
    }

    Ok(())
}

#[cfg(test)]
mod api_tests;
#[cfg(test)]
pub(crate) mod state_tests;

#[cfg(test)]
use crate::canvas::CanvasManager;
#[cfg(test)]
use crate::model_router::ModelRouter;
#[cfg(test)]
use crate::plugins::PluginManager;
