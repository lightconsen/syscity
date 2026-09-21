//! Scalar-optimizer, guardrail and verdict tuning for the eval loop.

use super::*;
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
