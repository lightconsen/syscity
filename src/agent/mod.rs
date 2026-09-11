//! Core Agent module for Syscity
//!
//! The Agent is the central orchestrator that handles conversations,
//! manages context, calls tools, and interacts with LLM providers.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};

use crate::channels::thread_binding::ThreadBindingManager;
use crate::providers::Provider;
use crate::tools::ToolRegistry;

/// Progress events during message processing
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// Started processing
    Started,
    /// Executing a tool
    ToolCalling { name: String, arguments: String },
    /// Tool execution completed
    ToolResult {
        name: String,
        result: String,
        data: Option<serde_json::Value>,
        execution_time_ms: u64,
    },
    /// Incremental chunk from a streaming tool
    ToolResultDelta {
        name: String,
        chunk: String,
        is_error: bool,
    },
    /// LLM is generating reasoning/thinking content
    Generating { content: Option<String> },
    /// LLM is streaming text content delta
    ContentDelta { text: String },
    /// Completed with final response
    ///
    /// `turn_id` is the stable per-turn identifier (from the observability
    /// collector). It is empty for turns that never produced a collector
    /// (e.g. prompt-injection-guard rejections). `usage` carries the
    /// provider-reported token/credit usage (`None` for guard rejections
    /// and cache hits).
    Completed {
        response: String,
        turn_id: String,
        usage: Option<crate::providers::Usage>,
    },
    /// Error occurred
    Error { message: String },
}

/// Callback type for progress updates
pub type ProgressCallback = Arc<
    dyn Fn(ProgressEvent) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

mod agent_builder;
mod agent_cache;
mod agent_config;
mod agent_lifecycle;
mod agent_setup;
mod engine;

pub use agent_builder::AgentBuilder;
pub use agent_cache::{CachedResponse, ResponseCache};
pub use agent_config::AgentConfig;

pub mod acp;
pub mod artifacts;
pub mod budget;
pub mod compaction;
pub mod compressor;
pub mod context;
pub mod cost_guard;
pub mod disk_budget;
pub mod group;
pub mod personality;
pub mod planner;
pub mod prompt_builder;
pub mod reflection;
pub mod route_resolution;
pub mod session;
pub mod session_files;
pub mod session_store;
pub mod subagent_registry;
pub mod todo;
pub mod transcript;
pub mod turns;

pub use acp::{
    AcpCommand, AcpController, AcpSessionStatus, ExecutionController, ExecutionMode, RuntimeState,
};
pub use artifacts::{Artifact, ArtifactStore, ArtifactStoreStats, ArtifactType};
pub use budget::{BudgetConfig, BudgetExhaustionAction, IterationBudget};
pub use compaction::{
    compute_context_hash, should_run_memory_flush, MemoryFlushConfig, SessionCompactionState,
    DEFAULT_MEMORY_FLUSH_FORCE_TRANSCRIPT_BYTES, DEFAULT_MEMORY_FLUSH_PROMPT,
    DEFAULT_MEMORY_FLUSH_RESERVE_TOKENS_FLOOR, DEFAULT_MEMORY_FLUSH_SOFT_TOKENS,
    DEFAULT_MEMORY_FLUSH_SYSTEM_PROMPT,
};
pub use compressor::{CompressionStats, CompressionStrategy, ContextCompressor};
pub use context::Context;
pub use cost_guard::CostGuard;
pub use disk_budget::{
    BudgetCategory, DiskBudgetError, DiskBudgetManager, EvictionStrategy, GlobalBudgetStats,
    SessionBudget, SessionBudgetStats,
};
pub use group::{
    GroupManagerStats, GroupMember, GroupRole, GroupSession, GroupSessionError, GroupSessionManager,
};
pub use personality::{
    seed_agent_personality, seed_agent_personality_sync, AgentPersonality, AgentRegistry,
    AgentTemplateParams, PersonalityContext, SharedAgentRegistry,
};
pub use planner::PersistedPlan;
pub use planner::{ActivePlan, TaskPlan, TaskPlanner};
pub use prompt_builder::{ConversationPhase, PromptBuilder, PromptContext, TaskType};
pub use route_resolution::{
    BindingCache, BindingMode, ConversationScope, ResolvedBinding, RouteResolution, RouteResolver,
    RouteRule,
};
pub use session::{
    AgentInstanceStatus, MultiAgentSession, SessionAgent, SessionManager, SessionMessage,
    SessionStatus, ThreadBinding,
};
pub use session_files::{SessionFileDir, SessionFileManager};
pub use subagent_registry::{SubagentMetrics, SubagentRegistry, SubagentRun, SubagentStatus};
pub use todo::{Task, TaskStatus, TodoStore};
pub use transcript::{
    render_transcript, Transcript, TranscriptFormat, TranscriptMessage, TranscriptStore,
    TranscriptStoreStats,
};
pub use turns::{Thread, ThreadManager, Turn, TurnState};

use self::session_store::SessionStore;

/// A clone-by-value cell for the agent's runtime configuration.
///
/// `Agent` derives `Clone` in several places; if the config field were a plain
/// `RwLock`, cloning would share the same lock, so an update through one clone
/// would leak into every other. This wrapper copies the inner value on clone
/// instead, so each `Agent` clone owns an independent snapshot while still
/// allowing `update_config(&self)` on a shared `&Agent`.
#[derive(Debug, Default)]
pub(crate) struct ConfigCell(std::sync::RwLock<AgentConfig>);

impl Clone for ConfigCell {
    fn clone(&self) -> Self {
        ConfigCell(std::sync::RwLock::new(self.snapshot()))
    }
}

impl From<AgentConfig> for ConfigCell {
    fn from(config: AgentConfig) -> Self {
        ConfigCell(std::sync::RwLock::new(config))
    }
}

impl ConfigCell {
    /// Copy the current config value (never held across an await point).
    fn snapshot(&self) -> AgentConfig {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Replace the stored config value.
    fn replace(&self, new_config: AgentConfig) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = new_config;
    }
}

#[derive(Clone)]
pub struct Agent {
    /// Agent configuration (runtime-updatable, copy-on-clone).
    config: ConfigCell,
    /// Stable agent identifier set at spawn time.
    ///
    /// Empty string when spawned without an explicit id (e.g. ephemeral
    /// subagents); used to tag turn observability records.
    pub(crate) agent_id: String,
    /// The LLM provider
    provider: Arc<dyn Provider>,
    /// Model name to use (overrides provider default)
    model: Option<String>,
    /// Tool registry
    tools: Arc<ToolRegistry>,
    /// Per-conversation Thread (replaces flat `contexts` map).
    /// Thread owns the Context AND the turn log — conversation continuity lives
    /// here.
    pub(crate) thread_map: Arc<Mutex<HashMap<String, Thread>>>,
    /// Session store for turn persistence (optional).
    session_store: Option<Arc<SessionStore>>,
    /// Session ID used as namespace for turn persistence.
    session_id: Option<String>,
    /// Shutdown signal
    shutdown_tx: Arc<RwLock<Option<mpsc::Sender<()>>>>,
    /// Memory manager for unified memory operations (retrieval, storage,
    /// compaction)
    memory_manager: Option<Arc<crate::memory::MemoryManager>>,
    /// Memory store for persistence (legacy, prefer memory_manager)
    pub(crate) memory_store: Option<Arc<dyn crate::memory::MemoryStore>>,
    /// Chat history store for conversation persistence (legacy, prefer
    /// memory_manager)
    chat_history: Option<Arc<dyn crate::memory::ChatHistoryStore>>,
    /// Session search for conversation history indexing
    session_search: Option<Arc<crate::memory::SessionSearch>>,
    /// Response cache for identical prompts
    response_cache: Arc<ResponseCache>,
    /// Task planner for automatic task decomposition
    task_planner: Arc<TaskPlanner>,
    /// Active plans per conversation
    active_plans: Arc<RwLock<std::collections::HashMap<String, ActivePlan>>>,
    /// Live cost guard — checked before every provider call.
    cost_guard: Option<Arc<CostGuard>>,
    /// Active skill trust level for the current invocation.
    /// 0 = Community, 1 = Trusted (default).
    /// Set by the gateway before RunSkill invocations; reset to Trusted
    /// afterward.
    active_skill_trust: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Skill manager for deterministic skill prefiltering.
    /// When set, skills are dynamically filtered based on user message triggers
    /// before being included in the system prompt.
    skill_manager: Option<Arc<RwLock<crate::skills::SkillManager>>>,
    /// Optional execution controller for pause/resume/step/cancel.
    /// Set by the ACP before dispatching a command; cleared afterward.
    execution_controller: Arc<RwLock<Option<Arc<ExecutionController>>>>,
    /// Optional max tool iteration override.
    /// Set by the ACP before dispatching a command; cleared afterward.
    max_tool_iterations_override: Arc<RwLock<Option<usize>>>,
    /// Transcript store for session conversation records.
    transcript_store: Option<Arc<crate::agent::TranscriptStore>>,
    /// Artifact store for session-bound artifacts (code, docs, links).
    artifact_store: Option<Arc<crate::agent::ArtifactStore>>,
    /// Disk budget manager for per-session storage quota.
    disk_budget: Option<Arc<crate::agent::DiskBudgetManager>>,
    /// Session file manager for isolated per-session file operations.
    session_file_manager: Option<Arc<crate::agent::SessionFileManager>>,
    /// Provider-specific extra parameters (e.g. thinking config) injected into
    /// completion requests.
    extra_params: Arc<RwLock<Option<serde_json::Value>>>,
    /// Optional model router for advanced routing, key rotation, and fallback.
    model_router: Option<Arc<crate::model_router::ModelRouter>>,
    /// Temporary model override set per-request (e.g. from OpenAI-compatible
    /// API). Takes precedence over `model`.
    model_override: Arc<RwLock<Option<String>>>,
    /// Per-conversation model ID (session-scoped model binding). Keyed by
    /// conversation/session id so concurrent sessions on this shared agent do
    /// not interfere. Takes precedence over `model_override`.
    session_models: Arc<RwLock<HashMap<String, String>>>,
    /// Directory for persisting active plans (JSON files).
    plans_dir: Option<std::path::PathBuf>,
    /// PII detector for output content filtering.
    pii_detector: Option<Arc<crate::security::PiiDetector>>,
    /// Optional computer adapter for desktop automation.
    computer_adapter: Option<Arc<dyn crate::computer::ComputerAdapter>>,
    /// Configuration for the computer use loop.
    computer_config: Option<crate::computer::LoopConfig>,
    /// Optional goal planner for complex multi-step tasks with DAG scheduling.
    pub(crate) goal_planner: Option<Arc<crate::planner::GoalPlanner>>,
    /// Retrospect engine for periodic trajectory reflection (background).
    retrospect_engine: Option<reflection::RetrospectEngine>,
    /// Turn counter for retrospect scheduling.
    retrospect_counter: Arc<AtomicU64>,
    /// Optional thread binding manager for tracking session/thread hierarchy
    /// with idle timeout, max age, and child-spawning policies.
    thread_binding_manager: Option<ThreadBindingManager>,
    /// Per-conversation concurrency guards to prevent reentrant processing
    /// of the same conversation_id.
    concurrency_guards: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    /// Online risk-signal checker for the badcase auto-collection pipeline.
    /// When set (with `pending_badcase_store`), every completed turn is
    /// scanned and flagged turns are inserted into the pending pool.
    risk_checker: Option<crate::eval::RiskSignalChecker>,
    /// Pending-badcase pool that online risk signals (and human 👎 votes)
    /// land in, awaiting confirmation.
    pending_badcase_store: Option<Arc<crate::eval::PendingBadcaseStore>>,
    /// 在线质量监控（§八）：高风险命中时用 LLM Judge 深评该 turn。
    ///
    /// Snapshot at spawn time from `GatewayConfig.eval.online_monitoring`; the
    /// judge trigger in `scan_turn_for_badcase` reads this without any lock.
    online_monitoring: crate::gateway::config::OnlineMonitoringConfig,
    /// 压缩质量门禁（§三）：当 `enabled` 时，turn 内低保留率压缩被当作在线
    /// 风险信号收集进 pending 池（additive，不改判）。`min_retention_ratio`
    /// 同时传给 `TurnMetricsCollector` 以量化 `quality_flag`。
    compression_quality: crate::gateway::config::CompressionQualityConfig,
    /// Production turn sampling store. When attached (and `sampling.enabled`),
    /// a subset of completed turns is persisted to `turn_samples` for the
    /// scoring / compression-gate / feedback / shadow-replay pipelines.
    sample_store: Option<Arc<crate::eval::TurnSampleStore>>,
    /// 生产流量在线采样（§…）：`enabled` + `sample_rate` snapshot at spawn time.
    /// Disabled by default so existing deployments never write samples.
    sampling: crate::gateway::config::OnlineSamplingConfig,
}

/// RAII guard that reinserts a Thread into `thread_map` on drop.
///
/// Prevents thread loss if processing panics between take-out and reinsertion.
struct ThreadGuard {
    map: Arc<Mutex<HashMap<String, Thread>>>,
    key: String,
    thread: Option<Thread>,
}

impl ThreadGuard {
    /// Take a thread from the map by key.
    async fn take(map: &Arc<Mutex<HashMap<String, Thread>>>, key: &str) -> Self {
        let mut guard = map.lock().await;
        let thread = guard.remove(key);
        ThreadGuard {
            map: map.clone(),
            key: key.to_string(),
            thread,
        }
    }

    /// Get a mutable reference to the held thread.
    #[allow(clippy::expect_used)]
    fn get_mut(&mut self) -> &mut Thread {
        self.thread
            .as_mut()
            .expect("ThreadGuard: thread already consumed")
    }

    /// Consume the guard and return the thread for explicit reinsertion.
    /// Drop will NOT reinsert since the thread is moved out.
    #[allow(clippy::expect_used)]
    fn into_thread(mut self) -> Thread {
        self.thread
            .take()
            .expect("ThreadGuard: thread already consumed")
    }
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        // On the normal path, `into_thread()` or `discard()` sets
        // `self.thread = None` before this guard is dropped, so nothing
        // happens here. On panic the thread is still present and we spawn
        // a task to reinsert it — the brief gap is acceptable for recovery.
        if let Some(thread) = self.thread.take() {
            let map = self.map.clone();
            let key = self.key.clone();
            tokio::spawn(async move {
                let mut guard = map.lock().await;
                guard.insert(key, thread);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::agent_cache::{are_tools_cacheable, is_obviously_time_sensitive};
    use super::*;

    // ── is_obviously_time_sensitive ───────────────────────────────────────────

    #[test]
    fn test_is_obviously_time_sensitive_positive() {
        assert!(is_obviously_time_sensitive("what time is it now"));
        assert!(is_obviously_time_sensitive("CURRENT TIME please"));
        assert!(is_obviously_time_sensitive("what's the time in Tokyo"));
        assert!(is_obviously_time_sensitive("现在几点了"));
        assert!(is_obviously_time_sensitive("当前时间是多少"));
        assert!(is_obviously_time_sensitive("现在时间呢"));
    }

    #[test]
    fn test_is_obviously_time_sensitive_negative() {
        assert!(!is_obviously_time_sensitive("hello"));
        assert!(!is_obviously_time_sensitive("what is the weather"));
        assert!(!is_obviously_time_sensitive("explain quantum computing"));
        assert!(!is_obviously_time_sensitive("what time zone is EST"));
    }

    // ── are_tools_cacheable ───────────────────────────────────────────────────

    #[test]
    fn test_are_tools_cacheable_all_cacheable() {
        assert!(are_tools_cacheable(&["search".to_string(), "read_file".to_string()]));
        assert!(are_tools_cacheable(&[]));
        assert!(are_tools_cacheable(&["grep".to_string()]));
    }

    #[test]
    fn test_are_tools_cacheable_non_cacheable() {
        assert!(!are_tools_cacheable(&["datetime".to_string()]));
        assert!(!are_tools_cacheable(&["time".to_string(), "read_file".to_string()]));
        assert!(!are_tools_cacheable(&["weather_current".to_string()]));
        assert!(!are_tools_cacheable(&["stock_price".to_string()]));
        assert!(!are_tools_cacheable(&["crypto_price".to_string()]));
        assert!(!are_tools_cacheable(&["my_clock_tool".to_string()]));
        assert!(!are_tools_cacheable(&["get_date_today".to_string()]));
    }

    // ── AgentConfig ───────────────────────────────────────────────────────────

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.max_context_tokens, 16384);
        assert_eq!(config.max_concurrent_tools, 5);
        assert_eq!(config.temperature, 0.7);
        assert_eq!(config.max_tokens, 2048);
        assert!(config.skills_prompt.is_none());
        assert!(config.max_turns.is_none());
        assert!(config.compaction_model.is_none());
        assert!(!config.system_prompt.is_empty());
    }

    #[test]
    fn test_agent_config_full_system_prompt_without_skills() {
        let config = AgentConfig {
            system_prompt: "base".to_string(),
            ..Default::default()
        };
        assert_eq!(config.full_system_prompt(), "base");
    }

    #[test]
    fn test_agent_config_full_system_prompt_with_skills() {
        let config = AgentConfig {
            system_prompt: "base".to_string(),
            skills_prompt: Some("skill1".to_string()),
            ..Default::default()
        };
        let prompt = config.full_system_prompt();
        assert!(prompt.contains("base"));
        assert!(prompt.contains("skill1"));
        assert!(prompt.contains("Skills"));
    }

    #[test]
    fn test_agent_config_serde_roundtrip() {
        let config = AgentConfig {
            system_prompt: "test prompt".to_string(),
            max_context_tokens: 8192,
            max_concurrent_tools: 3,
            temperature: 0.5,
            max_tokens: 1024,
            skills_prompt: Some("skills".to_string()),
            max_turns: Some(10),
            compaction_model: Some("claude-haiku".to_string()),
            workspace_dir: None,
            workspace_only: false,
            heartbeat: Some(crate::heartbeat::HeartbeatConfig {
                enabled: true,
                interval_seconds: 120,
                active_hours_start: "09:00".to_string(),
                active_hours_end: "18:00".to_string(),
                max_consecutive_idle: 5,
                model: Some("claude-haiku".to_string()),
                provider: Some("anthropic".to_string()),
            }),
            agent_id: None,
            reflection_config: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.max_context_tokens, 8192);
        assert_eq!(restored.temperature, 0.5);
        assert_eq!(restored.max_tokens, 1024);
        assert_eq!(restored.max_concurrent_tools, 3);
        assert_eq!(restored.max_turns, Some(10));
        assert_eq!(restored.compaction_model, Some("claude-haiku".to_string()));

        // Verify heartbeat roundtrip
        let hb = restored.heartbeat.unwrap();
        assert!(hb.enabled);
        assert_eq!(hb.interval_seconds, 120);
        assert_eq!(hb.active_hours_start, "09:00");
        assert_eq!(hb.active_hours_end, "18:00");
        assert_eq!(hb.max_consecutive_idle, 5);
        assert_eq!(hb.model, Some("claude-haiku".to_string()));
        assert_eq!(hb.provider, Some("anthropic".to_string()));
    }

    // ── ResponseCache ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_response_cache_empty() {
        let cache = ResponseCache::new(Duration::from_secs(60));
        let result = cache.get("user1", "conv1", "hello").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_response_cache_set_and_get() {
        let cache = ResponseCache::new(Duration::from_secs(60));
        cache
            .set("user1", "conv1", "hello", "world".to_string(), vec![])
            .await;
        let result = cache.get("user1", "conv1", "hello").await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().response, "world");
    }

    #[tokio::test]
    async fn test_response_cache_key_isolation() {
        let cache = ResponseCache::new(Duration::from_secs(60));
        cache
            .set("user1", "conv1", "hello", "world".to_string(), vec![])
            .await;
        // Different user
        assert!(cache.get("user2", "conv1", "hello").await.is_none());
        // Different conversation
        assert!(cache.get("user1", "conv2", "hello").await.is_none());
        // Different message
        assert!(cache.get("user1", "conv1", "bye").await.is_none());
    }

    #[tokio::test]
    async fn test_response_cache_ttl_expiration() {
        let cache = ResponseCache::new(Duration::from_millis(50));
        cache
            .set("user1", "conv1", "hello", "world".to_string(), vec![])
            .await;
        // Immediately available
        assert!(cache.get("user1", "conv1", "hello").await.is_some());
        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(cache.get("user1", "conv1", "hello").await.is_none());
    }

    #[tokio::test]
    async fn test_response_cache_cleanup() {
        let cache = ResponseCache::new(Duration::from_millis(50));
        cache
            .set("user1", "conv1", "hello", "world".to_string(), vec![])
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        cache.cleanup().await;
        // After cleanup, entry should be gone
        let cache_guard = cache.cache.read().await;
        assert!(cache_guard.is_empty());
    }

    #[test]
    fn test_response_cache_generate_key_consistency() {
        let key1 = ResponseCache::generate_key("u1", "c1", "hello");
        let key2 = ResponseCache::generate_key("u1", "c1", "hello");
        let key3 = ResponseCache::generate_key("u1", "c1", "hello ");
        assert_eq!(key1, key2);
        // Trimmed so trailing space doesn't matter
        assert_eq!(key1, key3);
    }

    #[test]
    fn test_response_cache_generate_key_uniqueness() {
        let key1 = ResponseCache::generate_key("u1", "c1", "hello");
        let key2 = ResponseCache::generate_key("u1", "c1", "world");
        let key3 = ResponseCache::generate_key("u2", "c1", "hello");
        assert_ne!(key1, key2);
        assert_ne!(key1, key3);
    }

    // ── AgentBuilder ──────────────────────────────────────────────────────────

    #[test]
    fn test_agent_builder_new() {
        let builder = AgentBuilder::new();
        assert!(builder.config.is_none());
        assert!(builder.provider.is_none());
        assert!(builder.tools.is_none());
    }

    #[test]
    fn test_agent_builder_default() {
        let builder: AgentBuilder = Default::default();
        assert!(builder.config.is_none());
    }

    #[test]
    fn test_agent_builder_chaining() {
        let builder = AgentBuilder::new()
            .config(AgentConfig::default())
            .skills("skill1".to_string())
            .provider(Arc::new(crate::providers::mock::MockProvider::new()))
            .tools(Arc::new(ToolRegistry::new()));
        assert!(builder.config.is_some());
        assert!(builder.provider.is_some());
        assert!(builder.tools.is_some());
    }

    #[test]
    fn test_agent_builder_build_without_provider_fails() {
        let builder = AgentBuilder::new().config(AgentConfig::default());
        let result = builder.build();
        match result {
            Err(e) => assert!(e.to_string().contains("Provider required")),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn test_agent_builder_build_success() {
        let builder = AgentBuilder::new()
            .config(AgentConfig::default())
            .provider(Arc::new(crate::providers::mock::MockProvider::new()))
            .tools(Arc::new(ToolRegistry::new()));
        let result = builder.build();
        assert!(result.is_ok());
    }

    #[test]
    fn test_agent_builder_skills_prompt() {
        let builder = AgentBuilder::new().skills("my skill".to_string());
        assert_eq!(builder.config.unwrap().skills_prompt, Some("my skill".to_string()));
    }

    // ── Agent ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_agent_set_and_read_skill_trust() {
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        );
        assert_eq!(agent.current_skill_trust(), crate::tools::SkillTrust::Trusted);

        agent.set_skill_trust(crate::tools::SkillTrust::Community);
        assert_eq!(agent.current_skill_trust(), crate::tools::SkillTrust::Community);

        agent.set_skill_trust(crate::tools::SkillTrust::Trusted);
        assert_eq!(agent.current_skill_trust(), crate::tools::SkillTrust::Trusted);
    }

    #[test]
    fn test_agent_update_config() {
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        );
        let mut new_config = AgentConfig::default();
        new_config.temperature = 0.3;
        new_config.max_tokens = 512;
        agent.update_config(new_config);
        let cfg = agent.config_snapshot();
        assert_eq!(cfg.temperature, 0.3);
        assert_eq!(cfg.max_tokens, 512);
    }

    #[test]
    fn test_agent_config_clone_copies_instead_of_sharing() {
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        );
        let cloned = agent.clone();
        // Updating the clone must not leak into the original.
        let mut cfg = AgentConfig::default();
        cfg.temperature = 0.1;
        cloned.update_config(cfg);
        assert_eq!(agent.config_snapshot().temperature, AgentConfig::default().temperature);
        assert_eq!(cloned.config_snapshot().temperature, 0.1);
    }

    #[test]
    fn test_agent_with_model() {
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        )
        .with_model("claude-sonnet-4-6".to_string());
        assert_eq!(agent.model, Some("claude-sonnet-4-6".to_string()));
    }

    #[test]
    fn test_agent_builder_with_all_options() {
        let builder = AgentBuilder::new()
            .config(AgentConfig::default())
            .provider(Arc::new(crate::providers::mock::MockProvider::new()))
            .tools(Arc::new(ToolRegistry::new()));
        let result = builder.build();
        assert!(result.is_ok());
    }

    #[test]
    fn test_progress_event_clone() {
        let event = ProgressEvent::Started;
        let cloned = event.clone();
        assert!(matches!(cloned, ProgressEvent::Started));

        let event2 = ProgressEvent::ToolCalling {
            name: "search".to_string(),
            arguments: "{}".to_string(),
        };
        let cloned2 = event2.clone();
        assert!(matches!(cloned2, ProgressEvent::ToolCalling { name, .. } if name == "search"));
    }

    #[test]
    fn test_progress_event_debug() {
        let event = ProgressEvent::Completed {
            response: "hi".to_string(),
            turn_id: "t1".to_string(),
            usage: None,
        };
        let debug = format!("{:?}", event);
        assert!(debug.contains("Completed"));
    }

    #[test]
    fn test_cached_response_clone() {
        let entry = CachedResponse {
            response: "hello".to_string(),
            created_at: SystemTime::now(),
            tools_used: vec!["tool1".to_string()],
        };
        let cloned = entry.clone();
        assert_eq!(cloned.response, "hello");
        assert_eq!(cloned.tools_used, vec!["tool1"]);
    }

    #[tokio::test]
    async fn test_agent_get_chat_history_without_store() {
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        );
        let history = agent.get_chat_history("conv1", 10).await.unwrap();
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn test_agent_get_last_conversation_without_store() {
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        );
        let last = agent.get_last_conversation("user1").await.unwrap();
        assert!(last.is_none());
    }

    // ── Workspace Dir Resolution ──────────────────────────────────────────────

    #[test]
    fn test_resolve_workspace_dir_explicit() {
        let mut config = AgentConfig::default();
        config.workspace_dir = Some(PathBuf::from("/tmp/workspace"));
        assert_eq!(config.resolve_workspace_dir(), PathBuf::from("/tmp/workspace"));
    }

    #[test]
    fn test_resolve_workspace_dir_with_tilde() {
        let mut config = AgentConfig::default();
        config.workspace_dir = Some(PathBuf::from("~/projects"));
        let resolved = config.resolve_workspace_dir();
        assert!(!resolved.to_string_lossy().contains("~"));
        assert!(resolved.to_string_lossy().contains("projects"));
    }

    #[test]
    fn test_resolve_workspace_dir_default_fallback() {
        let config = AgentConfig::default();
        let resolved = config.resolve_workspace_dir();
        // The root itself is the process default (a temp dir under cfg(test));
        // what this pins is that it resolves to the workspace, not an agent dir.
        assert!(resolved.to_string_lossy().contains("workspace"));
    }

    #[test]
    fn test_resolve_workspace_dir_default_agent_id() {
        let mut config = AgentConfig::default();
        config.agent_id = Some("default".to_string());
        let resolved = config.resolve_workspace_dir();
        // Should use the global workspace dir, not agents/default/workspace
        assert!(resolved.to_string_lossy().contains("workspace"));
        assert!(!resolved.to_string_lossy().contains("agents"));
    }

    #[test]
    fn test_resolve_workspace_dir_named_agent() {
        let mut config = AgentConfig::default();
        config.agent_id = Some("my-agent".to_string());
        let resolved = config.resolve_workspace_dir();
        assert!(resolved.to_string_lossy().contains("agents"));
        assert!(resolved.to_string_lossy().contains("my-agent"));
        assert!(resolved.to_string_lossy().contains("workspace"));
    }

    // ── Skill Injection ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_skill_manager_injection_into_build_fresh_context() {
        // Create skill manager and load built-in skills
        let mut skill_manager = crate::skills::SkillManager::new().await.unwrap();
        let loaded = skill_manager.load_all().await.unwrap();
        assert!(loaded > 0, "Expected built-in skills to be loaded");

        let skill_manager = Arc::new(RwLock::new(skill_manager));

        // Create agent with skill manager injected
        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        )
        .with_skill_manager(skill_manager);

        // The prompt carries the stable catalog regardless of message content.
        let ctx = agent
            .build_fresh_context("conv1", "user1", "what's the weather in Beijing")
            .await;

        let prompt = ctx.system_prompt();
        assert!(
            prompt.contains("## Available Skills"),
            "Expected '## Available Skills' catalog in prompt, got: {}",
            prompt
        );
        assert!(
            prompt.contains("**weather**")
                && prompt.contains("Get weather information for locations"),
            "Expected weather catalog row in prompt, got: {}",
            prompt
        );
        assert!(
            !prompt.contains("# Weather Skill"),
            "Skill bodies must not be inlined into the prompt"
        );
    }

    #[tokio::test]
    async fn test_skill_catalog_stable_regardless_of_message() {
        let mut skill_manager = crate::skills::SkillManager::new().await.unwrap();
        skill_manager.load_all().await.unwrap();
        let skill_manager = Arc::new(RwLock::new(skill_manager));

        let agent = Agent::new(
            AgentConfig::default(),
            Arc::new(crate::providers::mock::MockProvider::new()),
            Arc::new(ToolRegistry::new()),
        )
        .with_skill_manager(skill_manager);

        // Even a message matching no trigger gets the same catalog — the
        // prompt prefix no longer varies per message.
        let ctx = agent
            .build_fresh_context("conv2", "user1", "hello there")
            .await;

        let prompt = ctx.system_prompt();
        assert!(
            prompt.contains("## Available Skills"),
            "Catalog must be present for any message, got: {}",
            prompt
        );
    }
}

/// Runtime invariant checks contributed by this module's stores, aggregated
/// by `core::invariants::register_builtins` for `syscity invariants`.
pub(crate) fn session_store_invariant_checks() -> Vec<crate::core::invariants::Invariant> {
    session_store::session_store_invariant_checks()
}

pub(crate) use todo::todo_invariant_checks;
