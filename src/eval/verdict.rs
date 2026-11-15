//! Statistical-verdict layer (§十二 ⑤⑥ 纪律).
//!
//! Before a candidate is auto-applied, it must pass an **eval-harness +
//! bootstrap verdict**: the candidate's agent configuration is run through a
//! regression suite and [`crate::eval::compare_versions`] decides whether the
//! candidate is *statistically* `Improved` over the baseline. Only an
//! `Improved` candidate may be applied/adopted.
//!
//! The gate is deliberately pluggable:
//!
//! - [`CandidateVerifier`] is the gate interface. `Ok(None)` means "no harness
//!   evidence available" (no provider / no suite / disabled) — the caller is
//!   expected to treat that conservatively as a reject.
//! - [`NoopVerifier`] always reports no evidence (used when verdicts are
//!   enabled but no suite is configured, so the gate degrades to a
//!   conservative reject rather than a silent pass).
//! - [`HarnessCandidateVerifier`] is the production implementation: it builds a
//!   baseline and a candidate [`crate::agent::Agent`], runs the configured
//!   regression suite through the [`crate::eval::harness::EvalHarness`], and
//!   returns a [`CandidateVerdict`] carrying the bootstrap comparison. Any LLM
//!   / harness failure degrades to `Ok(None)` — a missing provider must never
//!   fail the optimizer run.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use tracing::{debug, warn};

use crate::agent::{Agent, AgentBuilder, AgentConfig};
use crate::eval::{compare_versions, ComparisonVerdict, VersionComparison};
use crate::gateway::GatewayState;
use crate::providers::Provider;

/// A candidate under verdict: the current value and the proposed value for one
/// tuning subject (a config dot-path or structural target).
#[derive(Debug, Clone)]
pub struct VerdictSubject {
    /// Target path, e.g. `default_agent.temperature` or a tool name.
    pub path: String,
    /// Current value (baseline) as JSON.
    pub current: serde_json::Value,
    /// Proposed value (candidate) as JSON.
    pub proposed: serde_json::Value,
}

/// The result of a successful harness + bootstrap verdict.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateVerdict {
    /// Bootstrap comparison of the candidate against the baseline.
    pub comparison: VersionComparison,
    /// Number of harness evaluations (tasks) that produced baseline evidence.
    pub baseline_trials: usize,
    /// Number of harness evaluations (tasks) that produced candidate evidence.
    pub candidate_trials: usize,
    /// The regression-suite id that was run.
    pub suite_id: String,
}

/// Pluggable statistical verdict for a candidate (§十二 ⑤⑥ 纪律).
///
/// `Ok(None)` means "no harness evidence available" (no provider / no suite /
/// disabled). Callers treat that as a conservative reject: a candidate must
/// have *evidence* that it is `Improved` before it may be applied/adopted.
#[async_trait]
pub trait CandidateVerifier: Send + Sync {
    async fn verify(&self, subject: &VerdictSubject) -> Result<Option<CandidateVerdict>, String>;
}

/// A verifier that always reports "no evidence". Used when verdicts are
/// enabled but no suite is configured — the gate then degrades to a
/// conservative reject instead of a silent pass.
#[derive(Debug, Default)]
pub struct NoopVerifier;

#[async_trait]
impl CandidateVerifier for NoopVerifier {
    async fn verify(&self, _subject: &VerdictSubject) -> Result<Option<CandidateVerdict>, String> {
        Ok(None)
    }
}

/// Production verifier: run a baseline and a candidate agent through a
/// regression suite and return the bootstrap comparison.
///
/// Every failure mode (missing provider, unknown path, missing suite, harness
/// run error) degrades to `Ok(None)` — a missing LLM must never fail the
/// optimizer run.
pub struct HarnessCandidateVerifier {
    state: Arc<GatewayState>,
    suite_id: String,
    trials: usize,
    iterations: usize,
    confidence: f64,
    /// Reserved: the harness currently runs without an LLM judge critic
    /// (`EvalHarness::new(agent, None)`), so this is stored for a future
    /// judge-backed suite.
    #[allow(dead_code)]
    judge_model: Option<String>,
    /// `evals/` root to read the suite from. `None` = the project's own
    /// `evals/`, which is what the gateway wants; a test pins a temp corpus so
    /// measuring a candidate never reads (or writes) the checked-in tree.
    evals_dir: Option<PathBuf>,
}

impl HarnessCandidateVerifier {
    /// Create a verifier for `suite_id` with the given per-task trial count,
    /// bootstrap iterations, and confidence level.
    pub fn new(
        state: Arc<GatewayState>,
        suite_id: String,
        trials: usize,
        iterations: usize,
        confidence: f64,
        judge_model: Option<String>,
    ) -> Self {
        Self {
            state,
            suite_id,
            trials: trials.max(1),
            iterations: iterations.max(1),
            confidence,
            judge_model,
            evals_dir: None,
        }
    }

    /// Read the suite from `dir` instead of the project's `evals/`.
    pub fn with_evals_dir(mut self, dir: PathBuf) -> Self {
        self.evals_dir = Some(dir);
        self
    }
}

#[async_trait]
impl CandidateVerifier for HarnessCandidateVerifier {
    async fn verify(&self, subject: &VerdictSubject) -> Result<Option<CandidateVerdict>, String> {
        // 1. Snapshot the config so the config lock is never held across an
        //    `.await` (the harness runs make LLM calls).
        let snapshot = {
            let cfg = self.state.config.read().await;
            ConfigSnapshot {
                default_agent: cfg.default_agent.clone(),
                workspace_dir: cfg.workspace_dir.clone(),
                workspace_only: cfg.workspace_only,
                model: cfg.model.clone(),
            }
        };

        // 2. Resolve a provider. No provider → no harness evidence (debug, not
        //    an error): the run degrades gracefully.
        let Ok(provider) = self
            .state
            .infra
            .model_router
            .create_default_provider()
            .await
        else {
            debug!(
                "verdict: no default provider available; no harness evidence for {}",
                subject.path
            );
            return Ok(None);
        };

        // 3. Build the baseline agent from the current config and a candidate
        //    agent with the proposed value applied to the matching field.
        let mut candidate_cfg = snapshot.default_agent.clone();
        if !apply_scalar_to_config(&mut candidate_cfg, &subject.path, &subject.proposed) {
            debug!("verdict: unknown scalar path '{}'; no harness evidence", subject.path);
            return Ok(None);
        }
        let baseline_agent =
            match self.build_agent(provider.clone(), &snapshot, snapshot.default_agent.clone()) {
                Ok(a) => a,
                Err(e) => {
                    warn!("verdict: failed to build baseline agent for {}: {}", subject.path, e);
                    return Ok(None);
                }
            };
        let candidate_agent = match self.build_agent(provider, &snapshot, candidate_cfg) {
            Ok(a) => a,
            Err(e) => {
                warn!("verdict: failed to build candidate agent for {}: {}", subject.path, e);
                return Ok(None);
            }
        };

        // 4. Load the regression suite.
        //
        // `badcases` is the collected corpus, not a manifest: its
        // `suites/badcases.yaml` intentionally carries `tasks: []` because the
        // tasks are the YAML files under `badcases/`, written when a person
        // confirms a pending row. Reading the manifest for it would hand the
        // gate an empty suite, which passes vacuously and makes every
        // candidate look "not improved" — the opposite of measuring a
        // regression. So that one suite id is resolved through the collector.
        let evals_dir = self
            .evals_dir
            .clone()
            .unwrap_or_else(crate::eval::loader::default_evals_dir);
        let suite = match self.suite_id.as_str() {
            crate::eval::recycle::BADCASE_SUITE_ID => {
                match crate::eval::recycle::load_badcase_suite(&evals_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        debug!(
                            "verdict: failed to load the badcase corpus: {}; no harness evidence",
                            e
                        );
                        return Ok(None);
                    }
                }
            }
            _ => {
                let suite_path = evals_dir
                    .join("suites")
                    .join(format!("{}.yaml", self.suite_id));
                match crate::eval::loader::load_suite(&suite_path, &self.suite_id) {
                    Ok(s) => s,
                    Err(e) => {
                        debug!(
                            "verdict: failed to load suite '{}': {}; no harness evidence",
                            self.suite_id, e
                        );
                        return Ok(None);
                    }
                }
            }
        };
        // An empty suite has nothing to compare: every task list is empty, so
        // the bootstrap would receive zero observations. Say so instead of
        // letting the gate report a non-improvement it never measured.
        if suite.tasks.is_empty() {
            debug!("verdict: suite '{}' has no tasks; no harness evidence", self.suite_id);
            return Ok(None);
        }

        // 5. Run every task for the baseline and the candidate, recording
        //    per-task pass/fail (pass_rate > 0.0).
        let baseline_harness = crate::eval::harness::EvalHarness::new(baseline_agent, None);
        let candidate_harness = crate::eval::harness::EvalHarness::new(candidate_agent, None);
        let mut baseline_passes = Vec::with_capacity(suite.tasks.len());
        let mut candidate_passes = Vec::with_capacity(suite.tasks.len());
        for task in &suite.tasks {
            match baseline_harness.run(task.clone(), self.trials).await {
                Ok(summary) => baseline_passes.push(summary.pass_rate > 0.0),
                Err(e) => {
                    warn!("verdict: baseline run failed for task '{}': {}", task.id, e);
                    return Ok(None);
                }
            }
            match candidate_harness.run(task.clone(), self.trials).await {
                Ok(summary) => candidate_passes.push(summary.pass_rate > 0.0),
                Err(e) => {
                    warn!("verdict: candidate run failed for task '{}': {}", task.id, e);
                    return Ok(None);
                }
            }
        }

        // 6. Bootstrap the comparison and return the verdict.
        let comparison =
            compare_versions(&baseline_passes, &candidate_passes, self.iterations, self.confidence);
        Ok(Some(CandidateVerdict {
            comparison,
            baseline_trials: baseline_passes.len(),
            candidate_trials: candidate_passes.len(),
            suite_id: self.suite_id.clone(),
        }))
    }
}

/// Snapshot of the config fields the verifier needs to build agents. Cloned
/// under the config lock and dropped before any `.await`.
struct ConfigSnapshot {
    default_agent: AgentConfig,
    workspace_dir: Option<std::path::PathBuf>,
    workspace_only: bool,
    model: String,
}

impl HarnessCandidateVerifier {
    /// Build an [`Agent`] the same way `src/gateway/init/agents.rs` does.
    fn build_agent(
        &self,
        provider: Arc<dyn Provider + Send + Sync>,
        snapshot: &ConfigSnapshot,
        mut cfg: AgentConfig,
    ) -> Result<Arc<Agent>, String> {
        cfg.workspace_dir = snapshot.workspace_dir.clone();
        cfg.workspace_only = snapshot.workspace_only;
        AgentBuilder::new()
            .config(cfg)
            .provider(provider)
            .tools(self.state.tools.registry.clone())
            .model_router(self.state.infra.model_router.clone())
            .model(snapshot.model.clone())
            .planner_model(snapshot.model.clone())
            .skill_manager(self.state.tools.skills_manager.clone())
            .build()
            .map(Arc::new)
            .map_err(|e| e.to_string())
    }
}

/// Apply a proposed scalar value to the matching [`AgentConfig`] field.
/// Returns `false` for paths the scalar verifier does not know (structural
/// targets such as tool descriptions / system prompts have no scalar field and
/// therefore produce no harness evidence).
fn apply_scalar_to_config(cfg: &mut AgentConfig, path: &str, proposed: &serde_json::Value) -> bool {
    let Some(num) = proposed.as_f64() else {
        return false;
    };
    match path {
        "default_agent.temperature" => {
            cfg.temperature = num as f32;
            true
        }
        "default_agent.max_tokens" => {
            cfg.max_tokens = num as u32;
            true
        }
        "default_agent.max_context_tokens" => {
            cfg.max_context_tokens = num as usize;
            true
        }
        "default_agent.max_concurrent_tools" => {
            cfg.max_concurrent_tools = num as usize;
            true
        }
        _ => false,
    }
}

/// Whether a bootstrap comparison permits auto-apply (§十二 ⑤⑥ 纪律). Only a
/// statistically significant `Improved` result passes; regressions, flat
/// results, and insufficient data all reject.
pub(crate) fn verdict_allows_apply(comparison: &VersionComparison) -> bool {
    comparison.verdict == ComparisonVerdict::Improved
}

/// The reject reason for a non-improved comparison (used in decision traces /
/// run reports). `Improved` is the only passing verdict.
pub(crate) fn verdict_reason(comparison: &VersionComparison) -> &'static str {
    match comparison.verdict {
        ComparisonVerdict::Regressed => "verdict_regressed",
        ComparisonVerdict::NoSignificantChange | ComparisonVerdict::InsufficientData => {
            "verdict_not_improved"
        }
        ComparisonVerdict::Improved => "verdict_improved",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn improved_comparison() -> VersionComparison {
        VersionComparison {
            verdict: ComparisonVerdict::Improved,
            old_pass_rate: 0.5,
            new_pass_rate: 0.9,
            delta: 0.4,
            confidence_interval: (0.1, 0.7),
            bootstrap_iterations: 1000,
            computed_at: SystemTime::now(),
        }
    }

    fn regressed_comparison() -> VersionComparison {
        VersionComparison {
            verdict: ComparisonVerdict::Regressed,
            old_pass_rate: 0.9,
            new_pass_rate: 0.5,
            delta: -0.4,
            confidence_interval: (-0.7, -0.1),
            bootstrap_iterations: 1000,
            computed_at: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn noop_verifier_reports_no_evidence() {
        let subject = VerdictSubject {
            path: "default_agent.temperature".to_string(),
            current: serde_json::json!(0.7),
            proposed: serde_json::json!(0.8),
        };
        assert!(NoopVerifier.verify(&subject).await.unwrap().is_none());
    }

    #[test]
    fn verdict_selection_only_improved_applies() {
        assert!(verdict_allows_apply(&improved_comparison()));
        assert!(!verdict_allows_apply(&regressed_comparison()));

        let flat = VersionComparison {
            verdict: ComparisonVerdict::NoSignificantChange,
            ..improved_comparison()
        };
        assert!(!verdict_allows_apply(&flat));

        let insufficient = VersionComparison {
            verdict: ComparisonVerdict::InsufficientData,
            ..improved_comparison()
        };
        assert!(!verdict_allows_apply(&insufficient));

        // Reject reasons distinguish regression from mere non-improvement.
        assert_eq!(verdict_reason(&regressed_comparison()), "verdict_regressed");
        assert_eq!(verdict_reason(&flat), "verdict_not_improved");
        assert_eq!(verdict_reason(&insufficient), "verdict_not_improved");
        assert_eq!(verdict_reason(&improved_comparison()), "verdict_improved");
    }

    #[test]
    fn scalar_config_apply_handles_numeric_json() {
        let mut cfg = AgentConfig::default();
        // The optimizer produces JSON floats (json!(proposed)); as_u64() would
        // reject them, so the verifier must accept as_f64().
        assert!(apply_scalar_to_config(
            &mut cfg,
            "default_agent.temperature",
            &serde_json::json!(0.8)
        ));
        assert!((cfg.temperature - 0.8).abs() < 1e-6);

        assert!(apply_scalar_to_config(
            &mut cfg,
            "default_agent.max_tokens",
            &serde_json::json!(4096.0)
        ));
        assert_eq!(cfg.max_tokens, 4096);

        assert!(apply_scalar_to_config(
            &mut cfg,
            "default_agent.max_context_tokens",
            &serde_json::json!(32768.0)
        ));
        assert_eq!(cfg.max_context_tokens, 32768);

        assert!(apply_scalar_to_config(
            &mut cfg,
            "default_agent.max_concurrent_tools",
            &serde_json::json!(4.0)
        ));
        assert_eq!(cfg.max_concurrent_tools, 4);

        // Unknown path → no field is applied, config left unchanged.
        let before = cfg.clone();
        assert!(!apply_scalar_to_config(
            &mut cfg,
            "default_agent.system_prompt",
            &serde_json::json!("x")
        ));
        assert_eq!(cfg.temperature, before.temperature);
        assert_eq!(cfg.max_tokens, before.max_tokens);
        assert_eq!(cfg.max_context_tokens, before.max_context_tokens);
        assert_eq!(cfg.max_concurrent_tools, before.max_concurrent_tools);
    }
}
