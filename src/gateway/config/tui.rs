//! TUI-facing settings: theme, and the eval/self-tuning knobs.

use super::*;
/// Which palette the TUI should render with. `Auto` asks the terminal (an
/// OSC 11 query) and falls back to dark when no answer comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSetting {
    Dark,
    Light,
    #[default]
    Auto,
}

/// Client-facing TUI preferences, persisted in gateway config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// The theme setting the TUI resolves at startup.
    pub theme: ThemeSetting,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self { theme: ThemeSetting::Auto }
    }
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
