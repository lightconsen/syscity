//! Gateway configuration types.
//!
//! All `*Config` structs that make up the top-level [`GatewayConfig`] tree
//! plus their `Default` impls and serde defaults. Extracted from
//! `gateway/mod.rs` so the control-plane file isn't dominated by data
//! definitions.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::agent::AgentConfig;
use crate::channels::ChannelType;
use crate::mcp::McpSettings;
use crate::security::pairing::DmPolicy;

/// Gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Host to bind to
    pub host: String,
    /// Port for gateway control plane (serves API + WebSocket + SPA)
    pub port: u16,
    /// Default agent configuration
    pub default_agent: AgentConfig,
    /// Channel configurations
    pub channels: HashMap<String, ChannelConfig>,
    /// Vector memory configuration
    #[serde(default)]
    pub vector_memory: VectorMemoryConfig,
    /// Plugin system configuration
    #[serde(default)]
    pub plugins: PluginConfig,
    /// Hot reload configuration
    #[serde(default)]
    pub hot_reload: HotReloadConfig,
    /// ACP (Agent Control Plane) configuration
    #[serde(default)]
    pub acp: AcpConfig,
    /// Cron scheduler configuration
    #[serde(default)]
    pub cron: CronConfig,
    /// Heartbeat scheduler configuration
    #[serde(default)]
    pub heartbeat: crate::heartbeat::HeartbeatConfig,
    /// Security configuration
    #[serde(default)]
    pub security: SecurityConfig,
    /// Syscity Cloud integration (§2.7; feature `cloud`)
    #[cfg(feature = "cloud")]
    #[serde(default)]
    pub cloud: crate::cloud::config::CloudConfig,
    /// Storage adapter configuration
    #[serde(default)]
    pub storage: StorageConfig,
    /// LLM Provider configurations (provider name -> config)
    #[serde(default)]
    pub providers: HashMap<String, crate::model_router::ProviderConfig>,
    /// Default model name (e.g., "claude-3-sonnet-20240229", "qwen3.5-plus")
    #[serde(default = "default_model")]
    pub model: String,
    /// Model provider (e.g., "anthropic", "openai")
    #[serde(default = "default_model_provider")]
    pub model_provider: String,
    /// Per-agent model binding (agent_id -> concrete model ID). Sessions
    /// inherit this when they have no explicit model pin; empty map = global
    /// default.
    #[serde(default)]
    pub agent_models: HashMap<String, String>,
    /// Per-agent parameter overrides (agent_id -> overrides). A named agent's
    /// effective runtime config is its personality-derived base config with
    /// these fields layered on top. The default agent has no entry — it is
    /// configured directly through `default_agent`.
    #[serde(default)]
    pub agent_overrides: HashMap<String, AgentOverrides>,
    /// MCP server configurations (auto-connected on startup)
    #[serde(default)]
    pub mcp: McpSettings,
    /// Live spend and action-rate guard for LLM calls.
    #[serde(default)]
    pub cost_guard: CostGuardConfig,
    /// Workspace directory for file operations.
    /// All relative paths are resolved against this directory.
    /// When `workspace_only` is true, file operations are restricted to this
    /// directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<std::path::PathBuf>,
    /// When true, restrict file operations to `workspace_dir`.
    #[serde(default)]
    pub workspace_only: bool,
    /// Browser configuration (bridge, profiles, pool)
    #[cfg(feature = "browser")]
    #[serde(default)]
    pub browser: crate::config::BrowserConfig,
    /// Computer / desktop automation configuration
    #[serde(default)]
    pub computer: crate::config::ComputerConfig,
    /// Dream scheduler configuration for background memory consolidation
    #[serde(default)]
    pub dreaming: crate::config::MemoryDreamingConfig,
    /// Standing orders configuration (persistent background agent programs)
    #[serde(default)]
    pub standing_orders: crate::standing_orders::config::StandingOrderConfig,
    /// Capability set configuration (profile, scope, enabled sets)
    #[serde(default)]
    pub capabilities: crate::config::CapabilitiesConfig,
    /// Web search provider configuration.
    #[serde(default)]
    pub search: SearchConfig,
    /// Quality gate configuration for pre-release gating.
    #[serde(default)]
    pub quality_gate: crate::gateway::quality_gate::QualityGateConfig,
    /// Knowledge Base configuration for auto-ingest and watcher.
    #[serde(default)]
    pub knowledge_base: KnowledgeBaseConfig,
    /// Observability retention configuration for per-turn records.
    #[serde(default)]
    pub observe: ObserveConfig,
    /// Online update configuration (self-update via GitHub Releases).
    #[serde(default)]
    pub update: UpdateConfig,
    /// Harness self-tuning configuration (§十二: 可反馈/可复盘/可调参/护栏).
    #[serde(default)]
    pub eval: EvalConfig,
}

/// Harness self-tuning configuration.
///
/// Gates the background scalar optimizer that hot-updates the default agent's
/// scalar parameters (§十二 可调参). New fields added here must default to
/// disabled so existing configs never start tuning unexpectedly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvalConfig {
    /// Scalar optimizer that probes and hot-updates default-agent scalars.
    #[serde(default)]
    pub optimizer: ScalarOptimizerConfig,
    /// 回归集治理：难度分层 + 覆盖标签加权进套件（§八）。
    #[serde(default)]
    pub badcase_governance: BadcaseGovernanceConfig,
    /// 人工复核固定抽样率（§三）。
    #[serde(default)]
    pub human_review: HumanReviewConfig,
    /// 在线质量监控：高风险命中 → LLM Judge 深评（§八）。
    #[serde(default)]
    pub online_monitoring: OnlineMonitoringConfig,
    /// 压缩质量量化指标与门槛（§三）。
    #[serde(default)]
    pub compression_quality: CompressionQualityConfig,
    /// 结构提议器（工具描述 / prompt / SOP 改版候选，§十二 ⑤）。
    #[serde(default)]
    pub proposer: StructuralProposerConfig,
    /// 生产流量在线采样打分（§…）：持久化线上 turn 样本供后续打分/压缩门禁/
    /// feedback 聚合/影子回放流水线读取。
    #[serde(default)]
    pub sampling: OnlineSamplingConfig,
}

/// Configuration for the background scalar optimizer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalarOptimizerConfig {
    /// Master switch. When false, `eval.optimizer.run` reports `disabled` and
    /// the scheduler does not start.
    #[serde(default)]
    pub enabled: bool,
    /// Scheduling cadence between runs ("30m", "1h", "manual" = never).
    #[serde(default = "default_optimizer_cadence")]
    pub cadence: String,
    /// Maximum number of scalar candidates applied per run.
    #[serde(default = "default_optimizer_max_steps")]
    pub max_steps: u32,
    /// Perturbation delta for continuous scalars (temperature ± delta).
    #[serde(default = "default_optimizer_delta")]
    pub delta: f64,
    /// Allowed temperature range for probe candidates (安全区域锁定).
    #[serde(default = "default_optimizer_temp_bounds")]
    pub temperature_bounds: [f64; 2],
    /// Guardrails between candidate generation and application (§十二 护栏).
    #[serde(default)]
    pub guardrails: OptimizerGuardrailConfig,
    /// 统计判定：候选过治理后回归套件跑 harness + bootstrap，仅 Improved 出
    /// patch（§十二 ⑤⑥ 纪律）。关闭时回退到 guardrails 闸门行为。
    #[serde(default)]
    pub verdict: OptimizerVerdictConfig,
}

impl Default for ScalarOptimizerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cadence: default_optimizer_cadence(),
            max_steps: default_optimizer_max_steps(),
            delta: default_optimizer_delta(),
            temperature_bounds: default_optimizer_temp_bounds(),
            guardrails: OptimizerGuardrailConfig::default(),
            verdict: OptimizerVerdictConfig::default(),
        }
    }
}

/// Guardrail configuration for the scalar optimizer (§十二 护栏).
///
/// All fields default to safe/no-op values so existing configs never start
/// gating unexpectedly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizerGuardrailConfig {
    /// Master switch. When false the optimizer applies candidates with only
    /// the search-space fence (Phase 3 behavior).
    #[serde(default)]
    pub enabled: bool,
    /// Minimum shadow-eval pass rate a candidate must clear (used by the
    /// EvalHarness-based shadow evaluator; `0.0` disables the floor).
    #[serde(default = "default_guardrail_min_pass_rate")]
    pub min_shadow_pass_rate: f64,
    /// Circuit breaker: pause auto-apply after this many consecutive gate
    /// failures / rollbacks.
    #[serde(default = "default_guardrail_max_failures")]
    pub max_consecutive_failures: u32,
    /// Cooldown before an open breaker re-arms (seconds).
    #[serde(default = "default_guardrail_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Reject candidates when the global cost guard has exceeded its budget.
    #[serde(default = "default_guardrail_cost_enabled")]
    pub cost_guard_enabled: bool,
    /// Reject when the recent down-vote ratio exceeds this (online signal).
    #[serde(default = "default_guardrail_max_down_ratio")]
    pub max_down_ratio: f64,
    /// Minimum number of votes required before the down-ratio gate fires.
    #[serde(default = "default_guardrail_min_votes")]
    pub min_votes: u32,
    /// Reject when this many `online:risk` badcases accumulate in the window.
    #[serde(default = "default_guardrail_max_online_risks")]
    pub max_online_risks: u32,
    /// Online-signal observation window (hours).
    #[serde(default = "default_guardrail_window_hours")]
    pub window_hours: u64,
}

impl Default for OptimizerGuardrailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_shadow_pass_rate: default_guardrail_min_pass_rate(),
            max_consecutive_failures: default_guardrail_max_failures(),
            cooldown_secs: default_guardrail_cooldown_secs(),
            cost_guard_enabled: default_guardrail_cost_enabled(),
            max_down_ratio: default_guardrail_max_down_ratio(),
            min_votes: default_guardrail_min_votes(),
            max_online_risks: default_guardrail_max_online_risks(),
            window_hours: default_guardrail_window_hours(),
        }
    }
}

fn default_guardrail_min_pass_rate() -> f64 {
    0.7
}

fn default_guardrail_max_failures() -> u32 {
    2
}

fn default_guardrail_cooldown_secs() -> u64 {
    300
}

fn default_guardrail_cost_enabled() -> bool {
    true
}

fn default_guardrail_max_down_ratio() -> f64 {
    0.5
}

fn default_guardrail_min_votes() -> u32 {
    10
}

fn default_guardrail_max_online_risks() -> u32 {
    3
}

fn default_guardrail_window_hours() -> u64 {
    24
}

/// 标量优化器的统计判定（§十二 ⑤⑥ 纪律）。
///
/// 开启后，候选会经治理后回归套件跑 harness（多 trial）+ `compare_versions`
/// bootstrap 判定，**仅 `Improved` 出 patch**；关闭（默认）时回退到 guardrails
/// 闸门行为（启发式候选 + 三闸守护）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizerVerdictConfig {
    /// 统计判定主开关。
    #[serde(default)]
    pub enabled: bool,
    /// 治理后回归套件 id（如 `"badcases"`）。`None` = 不跑 harness（回退闸门）。
    #[serde(default)]
    pub suite: Option<String>,
    /// 每任务 trial 数（bootstrap 需要 ≥2）。
    #[serde(default = "default_verdict_trials")]
    pub trials: usize,
    /// bootstrap 重采样次数。
    #[serde(default = "default_verdict_iterations")]
    pub bootstrap_iterations: usize,
    /// 置信水平（默认 0.95）。
    #[serde(default = "default_verdict_confidence")]
    pub confidence_level: f64,
    /// 线上回放 shadow 判定（§十二 ⑧ · N=1）：开启后候选经采样真实 turn 回放 +
    /// bootstrap 判定，仅 `Improved` 出 patch（优先于 `suite` harness 判定）。
    /// 默认关。
    #[serde(default)]
    pub replay_shadow: bool,
}

impl Default for OptimizerVerdictConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            suite: None,
            trials: default_verdict_trials(),
            bootstrap_iterations: default_verdict_iterations(),
            confidence_level: default_verdict_confidence(),
            replay_shadow: false,
        }
    }
}

fn default_verdict_trials() -> usize {
    2
}

fn default_verdict_iterations() -> usize {
    1000
}

fn default_verdict_confidence() -> f64 {
    0.95
}

/// 结构提议器配置（§十二 ⑤：工具描述 / prompt / SOP 改版）。
///
/// 开启 `verdict_enabled` 后，候选经 harness + bootstrap verdict，仅 `Improved`
/// 采纳；关闭时回退到确定性启发 `judge`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralProposerConfig {
    /// 统计判定开关：候选跑 harness + bootstrap verdict。
    #[serde(default)]
    pub verdict_enabled: bool,
    /// 回归套件 id。
    #[serde(default)]
    pub suite: Option<String>,
    #[serde(default = "default_verdict_trials")]
    pub trials: usize,
    #[serde(default = "default_verdict_iterations")]
    pub bootstrap_iterations: usize,
    #[serde(default = "default_verdict_confidence")]
    pub confidence_level: f64,
    /// 单次提议候选上限（cap）。
    #[serde(default = "default_proposer_max_candidates")]
    pub max_candidates: usize,
}

impl Default for StructuralProposerConfig {
    fn default() -> Self {
        Self {
            verdict_enabled: false,
            suite: None,
            trials: default_verdict_trials(),
            bootstrap_iterations: default_verdict_iterations(),
            confidence_level: default_verdict_confidence(),
            max_candidates: default_proposer_max_candidates(),
        }
    }
}

fn default_proposer_max_candidates() -> usize {
    4
}

/// 回归集治理：难度分层 + 覆盖标签加权（§八）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadcaseGovernanceConfig {
    /// 难度标签 → trial 数乘子（如 `{"hard": 2.0, "medium": 1.5}`）。
    /// 命中的 badcase 按乘子放大其 trial 数（加权进套件）。
    #[serde(default)]
    pub difficulty_weights: HashMap<String, f64>,
    /// 未命中难度标签的默认权重。
    #[serde(default = "default_governance_default_weight")]
    pub default_weight: f64,
}

impl Default for BadcaseGovernanceConfig {
    fn default() -> Self {
        Self {
            difficulty_weights: HashMap::new(),
            default_weight: default_governance_default_weight(),
        }
    }
}

fn default_governance_default_weight() -> f64 {
    1.0
}

/// 人工复核固定抽样率（§三）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HumanReviewConfig {
    /// 固定抽样率 (0.0–1.0)。`Some(rate)` 时普通 case 也按 `rate` 抽样送人工
    /// 复核；`None`（默认）维持只兜低置信/冲突。
    #[serde(default)]
    pub sampling_rate: Option<f64>,
}

/// 在线质量监控（§八 在线质量监控）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineMonitoringConfig {
    /// 高风险命中 → 触发 LLM Judge 深评。
    #[serde(default)]
    pub enabled: bool,
    /// 单 turn 命中风险数 ≥ 该值触发 LLM Judge。
    #[serde(default = "default_monitoring_risk_threshold")]
    pub llm_judge_risk_threshold: usize,
    /// Judge 模型覆盖（默认用 critic 模型）。
    #[serde(default)]
    pub judge_model: Option<String>,
}

impl Default for OnlineMonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            llm_judge_risk_threshold: default_monitoring_risk_threshold(),
            judge_model: None,
        }
    }
}

fn default_monitoring_risk_threshold() -> usize {
    2
}

/// 压缩质量量化（§三）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionQualityConfig {
    /// 记录压缩质量指标到 CompressionObservation。
    #[serde(default)]
    pub enabled: bool,
    /// token 保留率低于该值时标记质量告警（0.0 = 关闭告警）。
    #[serde(default = "default_compression_min_retention")]
    pub min_retention_ratio: f64,
}

impl Default for CompressionQualityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_retention_ratio: default_compression_min_retention(),
        }
    }
}

fn default_compression_min_retention() -> f64 {
    0.5
}

/// 生产流量在线采样（§…）。
///
/// Persists a sampled subset of completed production turns to the
/// `turn_samples` store for the scoring / compression-gate / feedback
/// aggregation / shadow-replay pipelines. New fields added here must default
/// to disabled so existing configs never start writing samples unexpectedly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineSamplingConfig {
    /// Master switch. When false (default), no production turns are sampled.
    #[serde(default)]
    pub enabled: bool,
    /// Fraction of completed turns to keep in `[0.0, 1.0]`. `0.0` samples
    /// every turn (when `enabled`); a value in `(0.0, 1.0)` keeps roughly that
    /// fraction via a cheap deterministic per-turn skip.
    #[serde(default)]
    pub sample_rate: f64,
}

impl Default for OnlineSamplingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_rate: 0.0,
        }
    }
}

fn default_optimizer_cadence() -> String {
    "manual".to_string()
}

fn default_optimizer_max_steps() -> u32 {
    1
}

fn default_optimizer_delta() -> f64 {
    0.1
}

fn default_optimizer_temp_bounds() -> [f64; 2] {
    [0.0, 1.5]
}

/// Online update (self-update) configuration.
///
/// Controls whether the daemon checks for new releases and applies them via
/// `syscity update` / the web update flow. Set `enabled = false` to disable
/// the update endpoints entirely; `auto_check = false` disables the
/// background check at daemon startup (manual checks still work).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Master switch for online updates.
    pub enabled: bool,
    /// Check for new releases in the background at daemon startup.
    pub auto_check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_check: true,
        }
    }
}

/// Observability (per-turn records) retention configuration.
///
/// Backs the daemon-startup sweep that prunes old turn JSON files and SQLite
/// metric rows (`llm_calls` / `tool_call_metrics` / `turn_outcomes`). Manual
/// `syscity observe prune --older-than` overrides this for a one-off run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ObserveConfig {
    /// Keep turn records for this many days. Records older than this are
    /// pruned at daemon startup. `0` disables auto-cleanup.
    pub retention_days: u32,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self { retention_days: 30 }
    }
}

/// Per-agent parameter overrides layered on top of an agent's base config.
///
/// `None` fields mean "inherit the base value" (personality-derived for named
/// agents, `default_agent` for the default agent). All fields are optional so
/// an agent may override only the parameters it cares about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentOverrides {
    /// Default temperature for completions.
    pub temperature: Option<f32>,
    /// Maximum tokens per completion.
    pub max_tokens: Option<u32>,
    /// Hard cap on conversation turns kept in context.
    pub max_turns: Option<usize>,
    /// Maximum number of concurrent tool calls.
    pub max_concurrent_tools: Option<usize>,
    /// Restrict file operations to the agent's workspace directory.
    pub workspace_only: Option<bool>,
    /// Overrides the personality-derived system prompt when set.
    pub system_prompt: Option<String>,
    /// Maximum context window size (in tokens).
    pub max_context_tokens: Option<usize>,
}

impl AgentOverrides {
    /// True when no field is overridden.
    pub fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.max_tokens.is_none()
            && self.max_turns.is_none()
            && self.max_concurrent_tools.is_none()
            && self.workspace_only.is_none()
            && self.system_prompt.is_none()
            && self.max_context_tokens.is_none()
    }

    /// Overlay every non-`None` field onto a base `AgentConfig`.
    pub fn apply_to(&self, cfg: &mut AgentConfig) {
        if let Some(v) = self.temperature {
            cfg.temperature = v;
        }
        if let Some(v) = self.max_tokens {
            cfg.max_tokens = v;
        }
        if let Some(v) = self.max_turns {
            cfg.max_turns = Some(v);
        }
        if let Some(v) = self.max_concurrent_tools {
            cfg.max_concurrent_tools = v;
        }
        if let Some(v) = self.workspace_only {
            cfg.workspace_only = v;
        }
        if let Some(v) = self.system_prompt.clone() {
            cfg.system_prompt = v;
        }
        if let Some(v) = self.max_context_tokens {
            cfg.max_context_tokens = v;
        }
    }
}

/// Knowledge Base auto-ingest configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeBaseConfig {
    /// Auto-ingest stale/new KB documents on daemon startup.
    pub auto_ingest_on_startup: bool,
    /// Max concurrency for ingestion (default: 2).
    pub max_concurrent_ingests: usize,
}

impl Default for KnowledgeBaseConfig {
    fn default() -> Self {
        Self {
            auto_ingest_on_startup: false,
            max_concurrent_ingests: 2,
        }
    }
}

/// Default search provider name
fn default_search_provider() -> String {
    "duckduckgo".to_string()
}

/// Default provider API keys map
fn default_search_keys() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::new()
}

/// Default ordered list of search providers for fallback.
fn default_search_providers() -> Vec<String> {
    vec![default_search_provider()]
}

/// Web search provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Legacy single search provider name.
    /// Use `providers` for fallback ordering.
    #[serde(default = "default_search_provider")]
    pub provider: String,
    /// Ordered list of search providers to try.
    /// When empty, falls back to `[provider]`.
    #[serde(default = "default_search_providers")]
    pub providers: Vec<String>,
    /// Legacy single API key field.
    #[serde(default)]
    pub api_key: String,
    /// Per-provider API keys. Allows configuring multiple providers at once.
    /// The active provider uses the key from `keys[provider]` or falls back to
    /// `api_key`.
    #[serde(default = "default_search_keys")]
    pub keys: std::collections::HashMap<String, String>,
}

impl SearchConfig {
    /// Return the ordered list of provider names to try.
    /// Prefers `providers`; when empty, uses `[provider]`.
    pub fn provider_list(&self) -> Vec<String> {
        if self.providers.is_empty() {
            vec![self.provider.clone()]
        } else {
            self.providers.clone()
        }
    }

    /// Get the API key for a given provider name.
    /// Prefers `keys[provider]`, then the legacy `api_key`.
    pub fn api_key_for(&self, provider: &str) -> Option<String> {
        self.keys
            .get(provider)
            .cloned()
            .filter(|k| !k.is_empty())
            .or_else(|| self.api_key.clone().into())
            .filter(|k| !k.is_empty())
    }

    /// Resolve the configured provider list into provider instances.
    ///
    /// Unknown names are skipped with a warning; an empty result falls back
    /// to DuckDuckGo so the search tool is never left provider-less.
    pub fn to_providers(&self) -> Vec<crate::tools::web::SearchProvider> {
        let mut providers: Vec<crate::tools::web::SearchProvider> = self
            .provider_list()
            .iter()
            .filter_map(|name| {
                match crate::tools::web::SearchProvider::from_config_name(
                    name,
                    self.api_key_for(name),
                ) {
                    Some(p) => Some(p),
                    None => {
                        tracing::warn!("Unknown search provider '{}', skipping", name);
                        None
                    }
                }
            })
            .collect();
        if providers.is_empty() {
            providers.push(crate::tools::web::SearchProvider::DuckDuckGo);
        }
        providers
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: default_search_provider(),
            providers: default_search_providers(),
            api_key: String::new(),
            keys: default_search_keys(),
        }
    }
}

fn default_model() -> String {
    "claude-3-sonnet-20240229".to_string()
}

fn default_model_provider() -> String {
    "anthropic".to_string()
}

/// Embedding provider type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum EmbeddingProviderType {
    /// OpenAI API (requires API key)
    #[default]
    OpenAi,
    /// Local GGUF model (direct loading, no external service)
    LocalGguf,
}

/// Vector memory configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMemoryConfig {
    /// Enable vector memory / semantic search
    pub enabled: bool,
    /// Embedding provider type
    pub provider: EmbeddingProviderType,
    /// Embedding provider API key (e.g., OpenAI)
    pub embedding_api_key: Option<String>,
    /// Embedding model to use (for API providers)
    pub embedding_model: String,
    /// Embedding dimension
    pub embedding_dimension: usize,
    /// API base URL (for Azure, etc.)
    pub api_base_url: Option<String>,
    /// Local GGUF model path (for local-embeddings feature)
    pub local_model_path: Option<String>,
    /// Query transformer configuration (HyDE, etc.)
    #[serde(default)]
    pub query_transformer: QueryTransformerConfig,
    /// Cross-encoder reranker configuration
    #[serde(default)]
    pub reranker: RerankerConfig,
    /// Context-window-aware memory budgeting
    #[serde(default)]
    pub context_window: MemoryContextWindowConfig,
    /// Multi-Query expansion configuration
    #[serde(default)]
    pub multi_query: MultiQueryConfig,
    /// Embedding hyper-parameters (chunking / batching). Configurable so the
    /// harness can tune retrieval quality at runtime without code changes.
    #[serde(default)]
    pub embedding: EmbeddingParams,
}

impl Default for VectorMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled by default to avoid blocking on model download
            provider: EmbeddingProviderType::LocalGguf,
            embedding_api_key: None,
            embedding_model: "text-embedding-3-small".to_string(),
            embedding_dimension: 1536,
            api_base_url: None,
            local_model_path: Some(
                "hf:unsloth/embedding-gemma-2b-GGUF/embedding-gemma-2b-Q4_K_M.gguf".to_string(),
            ),
            query_transformer: QueryTransformerConfig::default(),
            reranker: RerankerConfig::default(),
            context_window: MemoryContextWindowConfig::default(),
            multi_query: MultiQueryConfig::default(),
            embedding: EmbeddingParams::default(),
        }
    }
}

/// Embedding hyper-parameters used by the vector-memory / knowledge-base
/// chunking pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingParams {
    /// Maximum chunk size for text splitting.
    #[serde(default = "default_embedding_chunk_size")]
    pub chunk_size: usize,
    /// Chunk overlap for sliding window (only used when `chunk_strategy` is
    /// `Fixed`).
    #[serde(default = "default_embedding_chunk_overlap")]
    pub chunk_overlap: usize,
    /// Batch size for embedding generation.
    #[serde(default = "default_embedding_batch_size")]
    pub batch_size: usize,
    /// Chunking strategy (defaults to the rag default: recursive 512).
    #[serde(default)]
    pub chunk_strategy: crate::rag::ChunkStrategy,
}

fn default_embedding_chunk_size() -> usize {
    512
}
fn default_embedding_chunk_overlap() -> usize {
    50
}
fn default_embedding_batch_size() -> usize {
    32
}

impl Default for EmbeddingParams {
    fn default() -> Self {
        Self {
            chunk_size: default_embedding_chunk_size(),
            chunk_overlap: default_embedding_chunk_overlap(),
            batch_size: default_embedding_batch_size(),
            chunk_strategy: crate::rag::ChunkStrategy::default(),
        }
    }
}

/// Query transformer configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryTransformerConfig {
    /// Enable HyDE (Hypothetical Document Embeddings) using the default LLM.
    pub enable_hyde: bool,
    /// Optional model override for HyDE generation.
    pub hyde_model: Option<String>,
}

/// Multi-Query expansion configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiQueryConfig {
    /// Enable Multi-Query expansion.
    #[serde(default)]
    pub enabled: bool,
    /// Number of LLM-generated sub-queries (not counting the original query).
    #[serde(default = "default_multi_query_variations")]
    pub num_variations: usize,
    /// Provider name for the expansion LLM (a key under `[providers.*]`).
    /// Pin an explicit direct/cheap provider here: unset falls back to the
    /// router's *first registered* provider, which is nondeterministic and
    /// may be a metered one (e.g. cloud), silently billing credits per turn.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model override for the expansion LLM. Unset → the provider's
    /// configured default model.
    #[serde(default)]
    pub model: Option<String>,
}

fn default_multi_query_variations() -> usize {
    3
}

impl Default for MultiQueryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            num_variations: 3,
            provider: None,
            model: None,
        }
    }
}

/// Cross-encoder reranker configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankerConfig {
    /// Enable cross-encoder reranking.
    pub enabled: bool,
    /// Cohere Rerank API key.
    pub api_key: Option<String>,
    /// Model name (e.g. "rerank-english-v3.0").
    pub model: String,
    /// Max results to return after reranking.
    pub top_k: usize,
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            model: "rerank-english-v3.0".to_string(),
            top_k: 10,
        }
    }
}

/// Context-window-aware memory budgeting configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryContextWindowConfig {
    /// Enable token-budget-aware memory filtering.
    pub enabled: bool,
    /// Maximum total tokens the LLM context can hold.
    pub max_tokens: usize,
    /// Tokens reserved for the LLM's response generation.
    pub reserved_for_response: usize,
    /// Minimum number of memories to retain, even if over budget.
    pub min_chunks: usize,
}

impl Default for MemoryContextWindowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tokens: 128_000,
            reserved_for_response: 4_096,
            min_chunks: 1,
        }
    }
}

/// Plugin system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    /// Enable plugin system
    pub enabled: bool,
    /// Auto-load plugins on startup
    pub auto_load: bool,
    /// Plugin directory path (None = default)
    pub plugin_dir: Option<String>,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_load: true,
            plugin_dir: None,
        }
    }
}

/// Hot reload configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotReloadConfig {
    /// Enable hot reload for configuration
    pub enabled: bool,
    /// Watch config files for changes
    pub watch_config: bool,
    /// Watch agent files for changes
    pub watch_agents: bool,
    /// Watch plugin files for changes
    pub watch_plugins: bool,
    /// Debounce duration in seconds
    pub debounce_seconds: u64,
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            watch_config: true,
            watch_agents: true,
            watch_plugins: true,
            debounce_seconds: 2,
        }
    }
}

/// ACP (Agent Control Plane) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    /// Enable subagent spawning
    pub enabled: bool,
    /// Maximum concurrent subagents
    pub max_subagents: usize,
    /// Default subagent timeout in seconds
    pub default_timeout_seconds: u64,
    /// Maximum iterations per ACP session execution
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
}

fn default_max_iterations() -> usize {
    100
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_subagents: 10,
            default_timeout_seconds: 300,
            max_iterations: default_max_iterations(),
        }
    }
}

/// Cron scheduler configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    /// Enable cron scheduler
    pub enabled: bool,
    /// Check interval in seconds
    pub check_interval_seconds: u64,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_seconds: 60,
        }
    }
}

/// Cost guard configuration — live spend and action-rate tracking.
///
/// Set `daily_limit_cents` and/or `hourly_action_limit` to non-zero values to
/// enable limits. Zero means unlimited (default).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostGuardConfig {
    /// Maximum daily LLM spend in cents (0 = unlimited).
    /// Example: 500 = $5.00/day cap.
    #[serde(default)]
    pub daily_limit_cents: u64,
    /// Maximum provider calls per hour across all agents (0 = unlimited).
    #[serde(default)]
    pub hourly_action_limit: u64,
}

/// Credential source precedence for tokens, API keys, and passwords.
///
/// Controls which source wins when both environment variables and the
/// configuration file supply the same credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialPrecedence {
    /// Environment variables take precedence over the config file.
    #[default]
    EnvFirst,
    /// The config file takes precedence over environment variables.
    ConfigFirst,
}

/// Security configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Enable security features (auth, rate limiting, security headers)
    pub enabled: bool,
    /// Require authentication for API access
    pub auth_required: bool,
    /// Require pairing for new users
    pub pairing_required: bool,
    /// Authentication mode
    #[serde(default)]
    pub auth_mode: crate::gateway::protocol::AuthMode,
    /// Shared secret token for simple authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_token: Option<String>,
    /// Rate limiting configuration
    pub rate_limit: RateLimitConfig,
    /// Enable security headers
    pub security_headers: bool,

    /// CORS configuration
    #[serde(default)]
    pub cors: crate::gateway::auth::CorsConfig,
    /// CSP configuration
    #[serde(default)]
    pub csp: crate::gateway::auth::CspConfig,
    /// Mention gating configuration
    #[serde(default)]
    pub mention_gating: crate::security::mention_gate::MentionGatingConfig,
    /// Allowed Tailscale tailnets (empty = any tailnet allowed when
    /// auth_mode=tailscale)
    #[serde(default)]
    pub allowed_tailnets: Vec<String>,
    /// Trusted proxy IPs for X-Forwarded-For header resolution
    #[serde(default)]
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// Tailscale whois cache TTL in seconds (default 300)
    #[serde(default = "default_tailscale_ttl")]
    pub tailscale_auth_ttl_secs: u64,
    /// Trusted proxy authentication configuration.
    #[serde(default)]
    pub trusted_proxy: crate::security::trusted_proxy::TrustedProxyConfig,
    /// Credential source precedence for tokens, API keys, and passwords.
    #[serde(default)]
    pub credential_precedence: CredentialPrecedence,
}

fn default_tailscale_ttl() -> u64 {
    300
}

/// Rate limiting configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Enable rate limiting
    pub enabled: bool,
    /// Maximum requests per window (legacy token bucket)
    pub capacity: u32,
    /// Refill rate (tokens per second) (legacy token bucket)
    pub refill_rate: f64,
    /// Use multi-tier sliding window rate limiting instead of token bucket
    #[serde(default)]
    pub multi_tier: bool,
    /// Global tier: overall API rate limit
    #[serde(default)]
    pub global: TierConfig,
    /// Per-authenticated-user rate limit
    #[serde(default)]
    pub per_user: TierConfig,
    /// Per-IP rate limit (for anonymous requests)
    #[serde(default)]
    pub per_ip: TierConfig,
    /// Per-endpoint rate limit
    #[serde(default)]
    pub per_endpoint: TierConfig,
    /// Shared-secret authentication scope rate limit.
    #[serde(default)]
    pub shared_secret: TierConfig,
    /// Device-token authentication scope rate limit.
    #[serde(default)]
    pub device_token: TierConfig,
    /// Webhook/hook authentication scope rate limit.
    #[serde(default)]
    pub hook_auth: TierConfig,
    /// Control-plane write operation rate limit.
    #[serde(default)]
    pub control_plane_write: TierConfig,
    /// Lockout configuration for repeated failures.
    #[serde(default)]
    pub lockout: crate::security::sliding_window::LockoutConfig,
    /// Skip rate limiting for loopback addresses.
    #[serde(default)]
    pub loopback_exempt: bool,
}

/// Single tier configuration for multi-tier rate limiting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    /// Enable this tier
    pub enabled: bool,
    /// Maximum requests per window
    pub capacity: u32,
    /// Window size in seconds
    pub window_secs: u64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: 100,
            window_secs: 60,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auth_required: false,
            pairing_required: false,
            auth_mode: crate::gateway::protocol::AuthMode::None,
            shared_token: None,
            rate_limit: RateLimitConfig::default(),
            security_headers: true,
            cors: crate::gateway::auth::CorsConfig::default(),
            csp: crate::gateway::auth::CspConfig::default(),
            mention_gating: crate::security::mention_gate::MentionGatingConfig::default(),
            allowed_tailnets: Vec::new(),
            trusted_proxies: Vec::new(),
            tailscale_auth_ttl_secs: 300,
            trusted_proxy: crate::security::trusted_proxy::TrustedProxyConfig::default(),
            credential_precedence: CredentialPrecedence::default(),
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: 100,
            refill_rate: 10.0,
            multi_tier: true,
            global: TierConfig {
                enabled: true,
                capacity: 1000,
                window_secs: 60,
            },
            per_user: TierConfig {
                enabled: true,
                capacity: 100,
                window_secs: 60,
            },
            per_ip: TierConfig {
                enabled: true,
                capacity: 30,
                window_secs: 60,
            },
            per_endpoint: TierConfig {
                enabled: false,
                capacity: 50,
                window_secs: 60,
            },
            shared_secret: TierConfig {
                enabled: true,
                capacity: 200,
                window_secs: 60,
            },
            device_token: TierConfig {
                enabled: true,
                capacity: 60,
                window_secs: 60,
            },
            hook_auth: TierConfig {
                enabled: true,
                capacity: 300,
                window_secs: 60,
            },
            control_plane_write: TierConfig {
                enabled: true,
                capacity: 20,
                window_secs: 60,
            },
            lockout: crate::security::sliding_window::LockoutConfig::default(),
            loopback_exempt: true,
        }
    }
}

/// Storage adapter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage type: "memory", "file", "sqlite"
    pub storage_type: String,
    /// Base path for file/SQLite storage
    pub base_path: Option<String>,
    /// SQLite database URL (if using sqlite)
    pub database_url: Option<String>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            storage_type: "sqlite".to_string(),
            base_path: None,
            database_url: None,
        }
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 18080,
            default_agent: AgentConfig::default(),
            channels: HashMap::new(),
            vector_memory: VectorMemoryConfig::default(),
            plugins: PluginConfig::default(),
            hot_reload: HotReloadConfig::default(),
            acp: AcpConfig::default(),
            cron: CronConfig::default(),
            heartbeat: crate::heartbeat::HeartbeatConfig::default(),
            security: SecurityConfig::default(),
            #[cfg(feature = "cloud")]
            cloud: crate::cloud::config::CloudConfig::default(),
            storage: StorageConfig::default(),
            providers: HashMap::new(),
            model: default_model(),
            model_provider: default_model_provider(),
            agent_models: HashMap::new(),
            agent_overrides: HashMap::new(),
            mcp: McpSettings::default(),
            cost_guard: CostGuardConfig::default(),
            workspace_dir: None,
            workspace_only: true,
            #[cfg(feature = "browser")]
            browser: crate::config::BrowserConfig::default(),
            computer: crate::config::ComputerConfig::default(),
            dreaming: crate::config::MemoryDreamingConfig::default(),
            standing_orders: crate::standing_orders::config::StandingOrderConfig::default(),
            capabilities: crate::config::CapabilitiesConfig::default(),
            search: SearchConfig::default(),
            quality_gate: crate::gateway::quality_gate::QualityGateConfig::default(),
            knowledge_base: KnowledgeBaseConfig::default(),
            observe: ObserveConfig::default(),
            update: UpdateConfig::default(),
            eval: EvalConfig::default(),
        }
    }
}

/// Channel-specific configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Channel type
    pub channel_type: ChannelType,
    /// Whether channel is enabled
    pub enabled: bool,
    /// Channel-specific credentials/tokens
    pub credentials: HashMap<String, String>,
    /// DM policy: open, pairing, or allowlist
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Require explicit mention in group chats (ignored for DMs)
    #[serde(default)]
    pub require_mention: bool,
    /// Allowlist of users/numbers (for allowlist policy)
    #[serde(default)]
    pub allow_from: Vec<String>,
    /// Blocklist of users/numbers
    #[serde(default)]
    pub block_from: Vec<String>,
    /// Agent ID to route to (None = default)
    pub agent_id: Option<String>,
}

impl ChannelConfig {
    /// Create a new channel config with open policy (default).
    pub fn new(channel_type: ChannelType) -> Self {
        Self {
            channel_type,
            enabled: true,
            credentials: HashMap::new(),
            dm_policy: DmPolicy::Open,
            require_mention: false,
            allow_from: Vec::new(),
            block_from: Vec::new(),
            agent_id: None,
        }
    }

    /// Set the DM policy.
    pub fn with_dm_policy(mut self, policy: DmPolicy) -> Self {
        self.dm_policy = policy;
        self
    }

    /// Set the allowlist.
    pub fn with_allow_from(mut self, allow_from: Vec<String>) -> Self {
        self.allow_from = allow_from;
        self
    }

    /// Check if a user is in the allowlist.
    pub fn is_in_allowlist(&self, user_id: &str) -> bool {
        self.allow_from.iter().any(|a| a == user_id)
    }

    /// Check if a user is blocked.
    pub fn is_blocked(&self, user_id: &str) -> bool {
        self.block_from.iter().any(|b| b == user_id)
    }
}

// ── Hot-reload snapshot/diff ─────────────────────────────────────────────────

/// Snapshot of hot-reloadable configuration values used to compute diffs
/// across reloads.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigSnapshot {
    /// Timestamp when the snapshot was taken
    pub timestamp: String,
    /// The hot-reloadable field values keyed by dotted path (e.g.
    /// "providers.openai.base_url")
    pub fields: HashMap<String, serde_json::Value>,
}

/// A single configuration field change detected during hot reload.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigChange {
    /// Dotted path of the changed field (e.g. "model", "providers.openai")
    pub path: String,
    /// Previous value (absent for newly added fields)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<serde_json::Value>,
    /// New value (absent for removed fields)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<serde_json::Value>,
}

/// Recursively rebuild a JSON value with object keys sorted, so a hash over it
/// is independent of `HashMap` iteration order. Defensive: `serde_json`'s
/// default `Map` is already a `BTreeMap`, but this survives a future
/// `preserve_order` unification.
fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<(String, serde_json::Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize_json(v)))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(pairs.into_iter().collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalize_json).collect())
        }
        other => other.clone(),
    }
}

/// Canonical SHA-256 (hex) of a config JSON value. The canonical form makes
/// the hash stable across `HashMap` insertion orders, so two equal configs
/// always hash equal — the basis for optimistic-locking revisions.
pub(crate) fn config_json_hash(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};

    let canonical = canonicalize_json(value);
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    format!("{digest:x}")
}

/// Current revision of the whole `GatewayConfig` — the same fingerprint the
/// gateway tool reports as `hash`, so both write surfaces agree on CAS.
///
/// Synchronous and lock-free: callers hold a read/write guard, and computing
/// the hash must not `.await` (respects `await_holding_lock` deny).
pub(crate) fn config_revision(config: &GatewayConfig) -> String {
    // All GatewayConfig fields implement Serialize; this cannot fail.
    #[allow(clippy::expect_used)]
    let value = serde_json::to_value(config).expect("GatewayConfig serialization cannot fail");
    config_json_hash(&value)
}

impl GatewayConfig {
    /// Find the provider name that owns a concrete model ID, if any.
    pub fn provider_for_model(&self, model_id: &str) -> Option<&str> {
        self.providers
            .iter()
            .find(|(_, cfg)| cfg.supports_model(model_id))
            .map(|(name, _)| name.as_str())
    }

    /// Merge the persisted per-agent overrides for `agent_id` into `base` in
    /// place. `base` is the agent's personality-derived config (named agents)
    /// or `default_agent` (default agent); agents with no override entry are
    /// left untouched.
    pub fn apply_agent_overrides(&self, agent_id: &str, base: &mut AgentConfig) {
        if let Some(overrides) = self.agent_overrides.get(agent_id) {
            overrides.apply_to(base);
        }
    }

    /// Apply a single per-agent override field from a `config.set` JSON value.
    ///
    /// `field` is the final path segment (e.g. `"temperature"`). A `Null`
    /// value (or an empty string for `system_prompt`) clears the override so
    /// the agent falls back to its base value. Returns `Ok(true)` when the
    /// stored override changed.
    pub fn apply_agent_override_field(
        &mut self,
        agent_id: &str,
        field: &str,
        value: &serde_json::Value,
    ) -> crate::Result<bool> {
        use crate::error::ConfigError;
        let invalid = |msg: String| ConfigError::InvalidValue {
            key: format!("agent_overrides.{}.{}", agent_id, field),
            message: msg,
        };

        let overrides = self
            .agent_overrides
            .entry(agent_id.to_string())
            .or_default();
        let changed = match field {
            "temperature" => {
                if value.is_null() {
                    let c = overrides.temperature.is_some();
                    overrides.temperature = None;
                    c
                } else {
                    let v = value
                        .as_f64()
                        .map(|f| f as f32)
                        .ok_or_else(|| invalid("temperature must be a number".into()))?;
                    let c = overrides.temperature != Some(v);
                    overrides.temperature = Some(v);
                    c
                }
            }
            "max_tokens" => {
                if value.is_null() {
                    let c = overrides.max_tokens.is_some();
                    overrides.max_tokens = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_tokens must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_tokens != Some(v);
                    overrides.max_tokens = Some(v);
                    c
                }
            }
            "max_turns" => {
                if value.is_null() {
                    let c = overrides.max_turns.is_some();
                    overrides.max_turns = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_turns must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_turns != Some(v);
                    overrides.max_turns = Some(v);
                    c
                }
            }
            "max_concurrent_tools" => {
                if value.is_null() {
                    let c = overrides.max_concurrent_tools.is_some();
                    overrides.max_concurrent_tools = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_concurrent_tools must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_concurrent_tools != Some(v);
                    overrides.max_concurrent_tools = Some(v);
                    c
                }
            }
            "workspace_only" => {
                if value.is_null() {
                    let c = overrides.workspace_only.is_some();
                    overrides.workspace_only = None;
                    c
                } else {
                    let v = value
                        .as_bool()
                        .ok_or_else(|| invalid("workspace_only must be a boolean".into()))?;
                    let c = overrides.workspace_only != Some(v);
                    overrides.workspace_only = Some(v);
                    c
                }
            }
            "system_prompt" => {
                // Empty string means "inherit the personality prompt".
                if value.is_null() || value.as_str().is_none_or(|s| s.is_empty()) {
                    let c = overrides.system_prompt.is_some();
                    overrides.system_prompt = None;
                    c
                } else {
                    let v = value
                        .as_str()
                        .ok_or_else(|| invalid("system_prompt must be a string".into()))?
                        .to_string();
                    let c = overrides.system_prompt != Some(v.clone());
                    overrides.system_prompt = Some(v);
                    c
                }
            }
            "max_context_tokens" => {
                if value.is_null() {
                    let c = overrides.max_context_tokens.is_some();
                    overrides.max_context_tokens = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_context_tokens must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_context_tokens != Some(v);
                    overrides.max_context_tokens = Some(v);
                    c
                }
            }
            other => {
                return Err(crate::SyscityError::Config(ConfigError::InvalidValue {
                    key: format!("agent_overrides.{}.{}", agent_id, other),
                    message: format!("Unknown agent parameter field: {}", other),
                }))
            }
        };

        // Drop empty override entries so they stop appearing in config.get and
        // don't linger as empty TOML tables. Applies even when this call made
        // no change — a clear on an already-empty agent would otherwise
        // re-create an empty entry via `entry().or_default()` above.
        if overrides.is_empty() {
            self.agent_overrides.remove(agent_id);
        }
        Ok(changed)
    }

    /// Remove all overrides for an agent (full reset to base config).
    pub fn clear_agent_overrides(&mut self, agent_id: &str) {
        self.agent_overrides.remove(agent_id);
    }

    /// Capture a snapshot of all hot-reloadable fields.
    pub fn snapshot(&self) -> ConfigSnapshot {
        let mut fields = HashMap::new();

        let json = serde_json::to_value(self).unwrap_or_default();
        let obj = json.as_object().cloned().unwrap_or_default();

        // Only capture fields that are actually hot-reloadable
        let reloadable_keys = [
            "security",
            "providers",
            "mcp",
            "hot_reload",
            "cost_guard",
            "capabilities",
            "computer",
            "workspace_dir",
            "workspace_only",
            "model",
            "model_provider",
            "dreaming",
            "standing_orders",
            "cron",
            "browser",
        ];

        for key in &reloadable_keys {
            if let Some(val) = obj.get(*key) {
                if !val.is_null() {
                    fields.insert(key.to_string(), val.clone());
                }
            }
        }

        ConfigSnapshot {
            timestamp: chrono::Utc::now().to_rfc3339(),
            fields,
        }
    }

    /// Compute the list of field-level changes between a previous snapshot
    /// and the current configuration.
    pub fn diff_since(&self, old: &ConfigSnapshot) -> Vec<ConfigChange> {
        let current = self.snapshot();
        let mut changes = Vec::new();

        let all_keys: std::collections::BTreeSet<&String> =
            old.fields.keys().chain(current.fields.keys()).collect();

        for key in all_keys {
            let old_val = old.fields.get(key);
            let new_val = current.fields.get(key);

            match (old_val, new_val) {
                (Some(a), Some(b)) if a == b => continue,
                (Some(_), Some(b)) => {
                    changes.push(ConfigChange {
                        path: key.clone(),
                        old_value: old_val.cloned(),
                        new_value: Some(b.clone()),
                    });
                }
                (Some(a), None) => {
                    changes.push(ConfigChange {
                        path: key.clone(),
                        old_value: Some(a.clone()),
                        new_value: None,
                    });
                }
                (None, Some(b)) => {
                    changes.push(ConfigChange {
                        path: key.clone(),
                        old_value: None,
                        new_value: Some(b.clone()),
                    });
                }
                (None, None) => {}
            }
        }

        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the auto-generated default config written by the desktop
    /// shell (and mobile hosts) must round-trip through the TOML parser. A
    /// hand-written template drifted out of sync with GatewayConfig's schema
    /// (missing `security.rate_limit`, `[model]` as a table instead of flat
    /// keys) and silently fell back to defaults on the next start.
    #[test]
    fn default_config_round_trips() {
        let toml_str =
            toml::to_string_pretty(&GatewayConfig::default()).expect("serialize default config");
        // The serialized form must use the flat model keys, not a [model] table.
        assert!(toml_str.contains("\nmodel = "), "flat model key missing");
        assert!(!toml_str.contains("\n[model]\n"), "[model] table should not exist");
        // And the security section must carry the required rate_limit table.
        assert!(
            toml_str.contains("[security.rate_limit]"),
            "security.rate_limit missing from default config"
        );

        let parsed: GatewayConfig =
            toml::from_str(&toml_str).expect("default config must re-parse");
        assert_eq!(parsed.model, default_model());
        assert_eq!(parsed.model_provider, default_model_provider());
        assert_eq!(parsed.security.auth_mode, crate::gateway::protocol::AuthMode::None);
        assert!(!parsed.security.auth_required);
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 18080);
    }

    #[test]
    fn provider_for_model_finds_owning_provider() {
        let mut config = GatewayConfig::default();
        config.providers.insert(
            "deepseek".to_string(),
            crate::model_router::ProviderConfig {
                provider_type: crate::model_router::ProviderType::OpenAi,
                models: vec!["deepseek-chat".to_string(), "deepseek-reasoner".to_string()],
                default_model: "deepseek-chat".to_string(),
                api_key: String::new().into(),
                api_keys: Vec::new(),
                auth_profile: None,
                oauth: None,
                base_url: None,
                timeout: std::time::Duration::from_secs(30),
                max_retries: 3,
                retry_delay_ms: 1000,
            },
        );
        assert_eq!(config.provider_for_model("deepseek-chat"), Some("deepseek"));
        assert_eq!(config.provider_for_model("deepseek-reasoner"), Some("deepseek"));
        assert_eq!(config.provider_for_model("gpt-4o"), None);
    }

    // ── agent_overrides ───────────────────────────────────────────────────────

    #[test]
    fn apply_agent_overrides_merges_non_none_fields() {
        let mut config = GatewayConfig::default();
        config.agent_overrides.insert(
            "coder".to_string(),
            AgentOverrides {
                temperature: Some(0.2),
                max_tokens: Some(4096),
                system_prompt: Some("You are a code reviewer".to_string()),
                ..Default::default()
            },
        );

        let mut base = AgentConfig::default();
        config.apply_agent_overrides("coder", &mut base);

        assert_eq!(base.temperature, 0.2);
        assert_eq!(base.max_tokens, 4096);
        assert_eq!(base.system_prompt, "You are a code reviewer");
        // Unset fields keep the base value.
        assert_eq!(base.max_concurrent_tools, AgentConfig::default().max_concurrent_tools);
    }

    #[test]
    fn apply_agent_overrides_no_entry_is_noop() {
        let config = GatewayConfig::default();
        let mut base = AgentConfig::default();
        config.apply_agent_overrides("ghost", &mut base);
        assert_eq!(base.temperature, AgentConfig::default().temperature);
    }

    #[test]
    fn apply_agent_override_field_roundtrip() {
        let mut config = GatewayConfig::default();

        assert!(config
            .apply_agent_override_field("coder", "temperature", &serde_json::json!(0.5))
            .unwrap());
        assert_eq!(config.agent_overrides.get("coder").unwrap().temperature, Some(0.5));

        // Same value again → no change.
        assert!(!config
            .apply_agent_override_field("coder", "temperature", &serde_json::json!(0.5))
            .unwrap());

        // null clears the field; the now-empty entry is dropped.
        assert!(config
            .apply_agent_override_field("coder", "temperature", &serde_json::Value::Null)
            .unwrap());
        assert!(!config.agent_overrides.contains_key("coder"));
    }

    #[test]
    fn apply_agent_override_field_empty_prompt_clears() {
        let mut config = GatewayConfig::default();
        config
            .apply_agent_override_field("coder", "system_prompt", &serde_json::json!("hi"))
            .unwrap();
        assert_eq!(
            config
                .agent_overrides
                .get("coder")
                .unwrap()
                .system_prompt
                .as_deref(),
            Some("hi")
        );
        config
            .apply_agent_override_field("coder", "system_prompt", &serde_json::json!(""))
            .unwrap();
        // Clearing the only override drops the whole entry.
        assert!(!config.agent_overrides.contains_key("coder"));
    }

    #[test]
    fn apply_agent_override_field_rejects_unknown_field() {
        let mut config = GatewayConfig::default();
        let res = config.apply_agent_override_field("coder", "bogus", &serde_json::json!(1));
        assert!(res.is_err());
    }

    #[test]
    fn clear_agent_overrides_resets_whole_agent() {
        let mut config = GatewayConfig::default();
        config
            .apply_agent_override_field("coder", "max_tokens", &serde_json::json!(512))
            .unwrap();
        assert!(config.agent_overrides.contains_key("coder"));
        config.clear_agent_overrides("coder");
        assert!(!config.agent_overrides.contains_key("coder"));
    }

    #[test]
    fn agent_overrides_serialize_roundtrip() {
        let mut config = GatewayConfig::default();
        config
            .apply_agent_override_field("coder", "max_context_tokens", &serde_json::json!(8192))
            .unwrap();

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: GatewayConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed
                .agent_overrides
                .get("coder")
                .unwrap()
                .max_context_tokens,
            Some(8192)
        );
    }

    /// Build two configs with identical content but HashMaps populated in
    /// different insertion orders (channels, search keys, agent models). Their
    /// revisions must be identical — the CAS fingerprint cannot depend on
    /// HashMap iteration order.
    #[test]
    fn config_revision_stable_across_insertion_order() {
        let mut a = GatewayConfig::default();
        for (name, ty) in [("a", "telegram"), ("b", "slack"), ("c", "discord")] {
            let mut ch = ChannelConfig::new(match ty {
                "telegram" => crate::channels::ChannelType::Telegram,
                "slack" => crate::channels::ChannelType::Slack,
                _ => crate::channels::ChannelType::Discord,
            });
            ch.agent_id = Some(name.to_string());
            a.channels.insert(name.to_string(), ch);
        }
        a.search.keys.insert("k1".into(), "v1".into());
        a.search.keys.insert("k2".into(), "v2".into());
        a.agent_models.insert("m1".into(), "gpt-4o".into());
        a.agent_models.insert("m2".into(), "claude".into());

        let mut b = GatewayConfig::default();
        for (name, ty) in [("c", "discord"), ("b", "slack"), ("a", "telegram")] {
            let mut ch = ChannelConfig::new(match ty {
                "telegram" => crate::channels::ChannelType::Telegram,
                "slack" => crate::channels::ChannelType::Slack,
                _ => crate::channels::ChannelType::Discord,
            });
            ch.agent_id = Some(name.to_string());
            b.channels.insert(name.to_string(), ch);
        }
        b.search.keys.insert("k2".into(), "v2".into());
        b.search.keys.insert("k1".into(), "v1".into());
        b.agent_models.insert("m2".into(), "claude".into());
        b.agent_models.insert("m1".into(), "gpt-4o".into());

        assert_eq!(config_revision(&a), config_revision(&b));
    }

    #[test]
    fn config_revision_changes_when_config_changes() {
        let mut config = GatewayConfig::default();
        let before = config_revision(&config);
        config.default_agent.temperature = 0.5;
        let after = config_revision(&config);
        assert_ne!(before, after);
    }

    #[test]
    fn config_revision_matches_config_json_hash() {
        let config = GatewayConfig::default();
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(config_revision(&config), config_json_hash(&value));
    }
}
