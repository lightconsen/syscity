//! Domain-grouped sub-states for `GatewayState`.
//!
//! This module splits the monolithic flat gateway state into cohesive,
//! lifecycle-aligned sub-states (auth, agents, channels, memory, tools,
//! pipelines, events, infrastructure, sdk, scheduler). Each sub-state owns
//! the services that belong together, making initialization, shutdown, and
//! testing easier to reason about.
//!
//! Handlers access nested state directly, e.g. `state.auth.manager`,
//! `state.tools.registry`, `state.pipelines.inbound`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::acp::AcpControlPlane;
use crate::adapters::Storage;
use crate::agent::{
    session_store::SessionStore, AgentRegistry, ArtifactStore, CostGuard, DiskBudgetManager,
    GroupSessionManager, RouteResolver, SessionFileManager, SessionManager, TranscriptStore,
};
use crate::canvas::CanvasManager;
use crate::channels::snapshot::AccountSnapshotStore;
use crate::channels::{
    Channel, ChannelAcpBridge, ChannelExtensionRegistry, ChannelHealthMonitor, IncomingMessage,
};
use crate::config::hot_reload::HotReloadManager;
use crate::cron::cron::CronScheduler;
use crate::gateway::hooks::EventHookRegistry;
use crate::gateway::rate_limit::MultiTierRateLimiter;
use crate::gateway::task_registry::TaskRegistry;
use crate::gateway::AgentHandle;
use crate::gateway::RepairState;
use crate::gateway::{GatewayConfig, GatewayEvent};
use crate::heartbeat::{HeartbeatEvent, WakeRequest};
use crate::inbound::{AgentRouter, InboundPipeline, RoutedMessage};
use crate::mcp::McpManager;
use crate::memory::vector::VectorMemoryService;
use crate::memory::{DreamMetrics, MemoryManager, SessionSearch};
use crate::model_router::ModelRouter;
use crate::outbound::{OutboundPipeline, ReplyDispatcher, SideEffectExecutor, SseStreamer};
use crate::planner::TaskScheduler;
use crate::plugins::PluginManager;
use crate::providers::ProviderSdk;
use crate::secrets::SecretStoreHandle;
use crate::security::device_pairing::DevicePairingStore;
use crate::security::{
    mention_gate::MentionGate, pairing::PairingStore, persistent_audit::PersistentAuditLog,
    tailscale::TailscaleAuthenticator, trusted_proxy::TrustedProxyAuthenticator, AuthManager,
    RateLimiter,
};
use crate::skills::SkillManager;
use crate::tools::{
    approval::ApprovalQueue, ask_user::AskQueue, command_gate::CommandGate, ToolRegistry, ToolSdk,
};

/// Authentication, authorization, and security-related state.
pub struct AuthState {
    pub manager: Arc<AuthManager>,
    pub pairing_store: Arc<PairingStore>,
    pub device_pairing_store: Arc<DevicePairingStore>,
    pub tailscale_authenticator: Option<Arc<TailscaleAuthenticator>>,
    pub trusted_proxy_authenticator: Option<Arc<TrustedProxyAuthenticator>>,
    pub rate_limiter: Arc<RateLimiter>,
    pub multi_tier_rate_limiter: Arc<MultiTierRateLimiter>,
    pub audit_log: Arc<PersistentAuditLog>,
    pub command_gate: Arc<CommandGate>,
    pub mention_gate: Arc<MentionGate>,
}

/// Agent runtime, routing, and session management state.
pub struct AgentState {
    pub agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    /// IDs currently in the process of being spawned. A std::sync::Mutex is
    /// used so the guard can release the entry synchronously in Drop without
    /// spawning a detached cleanup task.
    pub pending_spawns: Arc<std::sync::Mutex<HashSet<String>>>,
    pub router: Arc<AgentRouter>,
    pub registry: Arc<RwLock<AgentRegistry>>,
    pub manager: Arc<RwLock<SessionManager>>,
    pub group_manager: Arc<RwLock<GroupSessionManager>>,
    pub store: Option<Arc<SessionStore>>,
    pub message_buffer: Arc<RwLock<HashMap<String, Vec<crate::gateway::BufferedMessage>>>>,
    pub route_resolver: Arc<RouteResolver>,
    pub cost_guard: Arc<CostGuard>,
    pub repair_state: Arc<RepairState>,
    pub acp: Arc<AcpControlPlane>,
    /// Active goal runners mapped by goal_id for cancellation support.
    pub goal_cancellers: Arc<RwLock<HashMap<String, CancellationToken>>>,
}

/// Channel adapters, extensions, and response dispatch state.
pub struct ChannelState {
    pub channels: Arc<RwLock<HashMap<String, Arc<dyn Channel>>>>,
    pub extensions: Arc<RwLock<ChannelExtensionRegistry>>,
    pub reply_dispatcher: Arc<ReplyDispatcher>,
    pub snapshot_store: Option<AccountSnapshotStore>,
    pub health_monitor: Option<Arc<ChannelHealthMonitor>>,
    pub acp_bridge: Option<Arc<ChannelAcpBridge>>,
    /// Session to channel mapping: session_id -> (channel_name,
    /// channel_specific_id).
    pub session_channels: Arc<RwLock<HashMap<String, (String, String)>>>,
    /// Webhook session storage: platform_key -> session_uuid.
    pub webhook_sessions: Arc<RwLock<HashMap<String, String>>>,
}

/// Memory, search, and background consolidation state.
pub struct MemoryState {
    pub vector: RwLock<Option<Arc<VectorMemoryService>>>,
    pub session_search: RwLock<Option<Arc<SessionSearch>>>,
    pub manager: Arc<RwLock<Option<Arc<MemoryManager>>>>,
    pub dream_scheduler: RwLock<Option<crate::memory::DreamScheduler>>,
    pub dream_metrics: Arc<DreamMetrics>,
    pub standing_order_manager: RwLock<Option<crate::standing_orders::StandingOrderManager>>,
    /// Knowledge Base ingestion manager (for auto-ingest and daemon watcher).
    pub kb_manager: RwLock<Option<Arc<crate::rag::ingestion::KnowledgeBaseManager>>>,
}

/// Tool registry, MCP, skills, and canvas state.
pub struct ToolState {
    pub registry: Arc<ToolRegistry>,
    pub mcp_manager: Arc<McpManager>,
    pub connector_manager: Arc<crate::mcp::ConnectorManager>,
    pub approval_queue: Arc<ApprovalQueue>,
    pub ask_queue: Arc<AskQueue>,
    pub skills_manager: Arc<RwLock<SkillManager>>,
    pub canvas_manager: Arc<CanvasManager>,
    pub computer_adapter: Arc<RwLock<Option<Arc<dyn crate::computer::ComputerAdapter>>>>,
    /// Shared handle for the PlannerTool — set during agent spawn.
    pub planner_handle: Arc<std::sync::RwLock<Option<Arc<crate::planner::GoalPlanner>>>>,
}

/// Message routing pipelines and streaming infrastructure state.
pub struct PipelineState {
    pub inbound: Arc<dyn InboundPipeline>,
    pub outbound: Arc<dyn OutboundPipeline>,
    pub side_effect_executor: Arc<SideEffectExecutor>,
    pub sse_streamer: Arc<SseStreamer>,
    pub routed_tx: mpsc::Sender<RoutedMessage>,
    pub inbound_entry: mpsc::Sender<IncomingMessage>,
}

/// Event broadcasting and hook registry state.
pub struct EventState {
    pub tx: broadcast::Sender<GatewayEvent>,
    pub log_tx: broadcast::Sender<String>,
    pub hook_registry: Arc<EventHookRegistry>,
}

/// Persistence, storage, plugins, and cross-cutting infrastructure state.
pub struct InfraState {
    pub storage: Arc<RwLock<dyn Storage>>,
    pub runtime_settings: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    pub transcript_store: Arc<TranscriptStore>,
    pub artifact_store: Arc<ArtifactStore>,
    pub disk_budget: Arc<DiskBudgetManager>,
    pub session_file_manager: Arc<SessionFileManager>,
    pub hot_reload: RwLock<Option<Arc<HotReloadManager>>>,
    pub plugin_manager: Arc<PluginManager>,
    pub model_router: Arc<ModelRouter>,
    /// CC-compatible shell hooks bridge (`~/.syscity/hooks.json`).
    pub shell_hooks: Arc<crate::hooks::ShellHookBridge>,
    /// Per-turn Like/Dislike feedback store (`feedback.vote`).
    pub feedback_store: Option<Arc<crate::gateway::FeedbackStore>>,
    /// Auto-collected badcase pool (online risk signals + human 👎).
    pub pending_badcase_store: Option<Arc<crate::eval::PendingBadcaseStore>>,
    /// Audit log of every harness tuning decision (apply/reject/rollback).
    pub decision_trace_store: Option<Arc<crate::eval::DecisionTraceStore>>,
    /// Production turn sampling store (online turn samples for scoring etc.).
    pub sample_store: Option<Arc<crate::eval::TurnSampleStore>>,
    /// Shared runtime state for the scalar optimizer (run status, pause flag).
    pub optimizer: Arc<crate::eval::OptimizerRuntime>,
    /// Browser bridge server (started when browser.bridge_enabled is true).
    #[cfg(feature = "browser")]
    pub browser_bridge: RwLock<Option<crate::browser::BrowserBridge>>,
}

/// Dynamic provider and tool SDK state.
pub struct SdkState {
    pub provider_sdk: Arc<RwLock<ProviderSdk>>,
    pub tool_sdk: Arc<RwLock<ToolSdk>>,
}

/// Background schedulers and heartbeat state.
pub struct SchedulerState {
    pub task_scheduler: RwLock<Option<Arc<Mutex<TaskScheduler>>>>,
    pub heartbeat_wake_tx: RwLock<Option<mpsc::Sender<WakeRequest>>>,
    pub heartbeat_event_tx: RwLock<Option<broadcast::Sender<HeartbeatEvent>>>,
    pub cron_scheduler: RwLock<Option<Arc<Mutex<CronScheduler>>>>,
}

/// Native device bridge state (mobile only).
///
/// The bridge is optional: on desktop it is always `None` and every
/// consumer degrades gracefully (`UNSUPPORTED_PLATFORM` for WS methods,
/// unavailable tools for the agent).
pub struct DeviceState {
    pub bridge: RwLock<Option<Arc<dyn crate::device::DeviceBridge>>>,
}

/// Online self-update (GitHub Releases) status and progress state.
pub struct UpdateState {
    /// Last checked release info, cached for the status TTL.
    pub status_cache: RwLock<Option<UpdateStatusCache>>,
    /// In-flight or last update run progress.
    pub progress: RwLock<UpdateProgress>,
    /// Total update checks performed (Prometheus counter).
    pub checks_total: std::sync::atomic::AtomicU64,
    /// Total update failures (Prometheus counter).
    pub failures_total: std::sync::atomic::AtomicU64,
}

impl UpdateState {
    /// Create an idle update state.
    pub fn new() -> Self {
        Self {
            status_cache: RwLock::new(None),
            progress: RwLock::new(UpdateProgress::default()),
            checks_total: std::sync::atomic::AtomicU64::new(0),
            failures_total: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl Default for UpdateState {
    fn default() -> Self {
        Self::new()
    }
}

/// A cached `UpdateInfo` plus the time it was fetched.
pub struct UpdateStatusCache {
    pub info: crate::update::UpdateInfo,
    pub checked_at: Instant,
}

/// Phase of the self-update run surfaced to the web UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    /// No update in progress.
    Idle,
    /// Checking GitHub for a newer release.
    Checking,
    /// Downloading and verifying the release tarball.
    Downloading,
    /// SHA-256 verification passed; staging the binary.
    Verifying,
    /// Replacing the running binary.
    Applying,
    /// Binary replaced; the daemon restart helper has been detached.
    Restarting,
    /// The last update attempt failed.
    Error,
}

/// Progress of the current (or last) self-update run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateProgress {
    pub phase: UpdatePhase,
    pub percent: u8,
    pub error: Option<String>,
    pub current: String,
    pub latest: Option<String>,
}

impl UpdateProgress {
    /// An idle progress with the given installed version.
    pub fn idle(current: impl Into<String>) -> Self {
        Self {
            phase: UpdatePhase::Idle,
            percent: 0,
            error: None,
            current: current.into(),
            latest: None,
        }
    }
}

impl Default for UpdateProgress {
    fn default() -> Self {
        Self::idle(crate::VERSION)
    }
}

/// Shared gateway state grouped by domain.
pub struct GatewayState {
    /// Configuration.
    ///
    /// Stored as `Arc<RwLock<Arc<GatewayConfig>>>` so that hot-reload can
    /// atomically replace the entire immutable config snapshot, and readers can
    /// take a cheap `Arc<GatewayConfig>` clone that remains consistent even
    /// while a reload is in progress.
    pub config: Arc<RwLock<Arc<GatewayConfig>>>,
    /// Gateway startup time for uptime calculations
    pub start_time: Instant,
    /// Path to the config file (for runtime persistence)
    pub config_path: Option<PathBuf>,
    /// Path to the MCP presets file (~/.syscity/mcp.toml)
    pub mcps_path: Option<PathBuf>,

    /// Secret-store instance handle (file backend + master key + memory cache).
    ///
    /// Constructed exactly once at gateway startup and threaded through the
    /// runtime so a process can host more than one gateway (or test) without
    /// sharing secret state. Replaces the former process-global singletons.
    pub secrets: Arc<SecretStoreHandle>,

    /// Centralized registry for all gateway background tasks.
    pub task_registry: Arc<TaskRegistry>,

    /// Shared shutdown signal used by command handlers and background tasks
    /// that only have access to `GatewayState`.
    pub shutdown_token: CancellationToken,

    pub auth: AuthState,
    pub agents: AgentState,
    pub channels: ChannelState,
    pub memory: MemoryState,
    pub tools: ToolState,
    pub pipelines: PipelineState,
    pub events: EventState,
    pub infra: InfraState,
    pub sdk: SdkState,
    pub scheduler: SchedulerState,
    pub device: DeviceState,
    pub update: UpdateState,
    /// Whether this gateway runs inside the desktop app (in-process). Captured
    /// from the `SYSCITY_EMBEDDED` env var at construction time; embedded
    /// instances must refuse self-replacement and defer to the desktop updater.
    pub embedded: bool,
}
