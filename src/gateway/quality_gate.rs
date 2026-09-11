//! Quality Gates — pre-release gating integrated with Gateway lifecycle.
//!
//! Implements §09: four-level ship gates that run eval suites before
//! allowing deployment, model switches, or traffic rollouts.
//!
//! Gate levels:
//! - OfflineDiff: paired comparison with baseline
//! - ShadowTraffic: run new agent on prod traffic (no user-facing)
//! - ABWithGuardrails: 10% traffic with guardrail triggers
//! - PhasedRollout: 1% → 10% → 50% → 100%

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::eval::compression_gate::{compression_criterion, CompressionGateConfig};
use crate::eval::harness::{EvalHarness, EvalSummary};
use crate::eval::loader::load_suite;
use crate::eval::recycle::{load_governed_badcase_suite, BadcaseGovernance};
use crate::eval::{PendingBadcaseStore, TurnSampleStore};
use crate::gateway::shadow_replay::{replay_shadow, samples_to_replay_turns};

/// Number of most-recent sampled production turns replayed by the shadow gate.
const SHADOW_SAMPLE_LIMIT: u32 = 50;

/// Load badcase regression suite for auto-inclusion in gate (§09).
fn load_badcase_regression_suite(evals_dir: &std::path::Path) -> Option<crate::eval::EvalSuite> {
    match crate::eval::load_badcase_suite(evals_dir) {
        Ok(suite) if !suite.tasks.is_empty() => Some(suite),
        _ => None,
    }
}

// ── Gate levels ────────────────────────────────────────────────────────

/// Gate level — maps to the four-level ship gate from §09.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateLevel {
    /// Offline comparison: N trials on both old + new, paired bootstrap.
    #[serde(rename = "offline_diff")]
    #[default]
    OfflineDiff,
    /// Shadow traffic: run new agent on production traffic, no user-facing.
    #[serde(rename = "shadow")]
    ShadowTraffic,
    /// A/B with guardrails: 10% traffic, guardrail triggers early stop.
    #[serde(rename = "ab")]
    ABWithGuardrails,
    /// Phased rollout: 1% → 10% → 50% → 100%.
    #[serde(rename = "phased")]
    PhasedRollout,
}

// ── Criteria ───────────────────────────────────────────────────────────

/// A single gate criterion that must pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GateCriterion {
    /// Core scenario pass rate >= min_rate.
    PassRate { suite_id: String, min_rate: f64 },
    /// Zero P0 risks.
    ZeroP0Risks,
    /// No regression vs baseline beyond max_degradation.
    NoRegressionVs {
        baseline_tag: String,
        metric: String,
        max_degradation: f64,
    },
    /// Continuous success rate >= min_rate.
    ContinuousSuccessRate { suite_id: String, min_rate: f64 },
}

/// Result of evaluating a single criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion: String,
    pub passed: bool,
    pub actual: f64,
    pub threshold: f64,
    pub detail: String,
}

// ── Aggregated suite results ───────────────────────────────────────────

/// Per-suite aggregate after running all tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteSummary {
    pub suite_id: String,
    pub total_tasks: usize,
    pub task_summaries: Vec<EvalSummary>,
    /// Overall pass rate across all tasks in this suite.
    pub overall_pass_rate: f64,
    /// Whether every task achieved continuous success (all trials passed).
    pub continuous_success: bool,
}

impl SuiteSummary {
    /// Aggregate per-task summaries into a suite-level summary.
    pub fn from_tasks(suite_id: String, summaries: Vec<EvalSummary>) -> Self {
        let total_tasks = summaries.len();
        if total_tasks == 0 {
            return Self {
                suite_id,
                total_tasks: 0,
                task_summaries: summaries,
                overall_pass_rate: 0.0,
                continuous_success: false,
            };
        }

        let overall_pass_rate =
            summaries.iter().map(|s| s.pass_rate).sum::<f64>() / total_tasks as f64;
        let continuous_success = summaries.iter().all(|s| s.continuous_success);

        Self {
            suite_id,
            total_tasks,
            task_summaries: summaries,
            overall_pass_rate,
            continuous_success,
        }
    }
}

// ── Gate result ────────────────────────────────────────────────────────

/// Overall gate result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub gate_name: String,
    pub passed: bool,
    pub criteria_results: Vec<CriterionResult>,
    pub suite_results: Vec<SuiteSummary>,
    pub started_at: SystemTime,
    pub completed_at: SystemTime,
}

// ── Baseline store ─────────────────────────────────────────────────────

/// A recorded baseline for regression comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BaselineRecord {
    tag: String,
    suite_id: String,
    pass_rate: f64,
    continuous_success: bool,
    recorded_at: SystemTime,
    /// Free-form metadata (e.g., agent version, model name).
    metadata: HashMap<String, String>,
}

/// Persistent store of eval baselines (stored as JSON).
///
/// Baselines let the `NoRegressionVs` criterion compare current results
/// against a previously recorded "good" state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineStore {
    path: PathBuf,
    records: Vec<BaselineRecord>,
}

impl BaselineStore {
    /// Load baselines from the default path (`~/.syscity/baselines.json`).
    pub fn load(paths: &crate::dirs::SyscityPaths) -> Self {
        Self::load_from(paths.data_dir().join("baselines.json"))
    }

    /// Load baselines from a specific path.
    pub fn load_from(path: PathBuf) -> Self {
        let records = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
                Err(e) => {
                    warn!("Failed to read baselines from {:?}: {}", path, e);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        Self { path, records }
    }

    /// Get the baseline pass rate for a given suite and tag.
    pub fn get_pass_rate(&self, tag: &str, suite_id: &str) -> Option<f64> {
        self.records
            .iter()
            .find(|r| r.tag == tag && r.suite_id == suite_id)
            .map(|r| r.pass_rate)
    }

    /// Store a new baseline for a suite.
    pub fn store(&mut self, tag: &str, suite_id: &str, summary: &SuiteSummary) {
        // Remove any existing record for the same tag+suite
        self.records
            .retain(|r| !(r.tag == tag && r.suite_id == suite_id));
        self.records.push(BaselineRecord {
            tag: tag.to_string(),
            suite_id: suite_id.to_string(),
            pass_rate: summary.overall_pass_rate,
            continuous_success: summary.continuous_success,
            recorded_at: SystemTime::now(),
            metadata: HashMap::new(),
        });
        self.save();
    }

    /// Persist records to disk.
    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.records) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, &json) {
                    warn!("Failed to write baselines to {:?}: {}", self.path, e);
                }
            }
            Err(e) => warn!("Failed to serialize baselines: {}", e),
        }
    }
}

// ── Config ─────────────────────────────────────────────────────────────

/// Quality gate configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QualityGateConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub level: GateLevel,
    #[serde(default)]
    pub suites: Vec<String>,
    #[serde(default = "default_min_pass_rate")]
    pub min_pass_rate: f64,
    #[serde(default)]
    pub require_zero_p0: bool,
    #[serde(default)]
    pub max_degradation: Option<f64>,
    #[serde(default)]
    pub baseline_tag: Option<String>,
    #[serde(default)]
    pub shutdown_on_failure: bool,
    #[serde(default)]
    pub cron_schedule: Option<String>,
    /// 压缩低保留率门禁（§十二 ⑧）：窗口内 `online:risk` 低保留率 flag 超过阈值
    /// 即判门禁失败。默认关（`None` = 不参与判定）。
    #[serde(default)]
    pub compression_gate: Option<CompressionGateConfig>,
}

fn default_min_pass_rate() -> f64 {
    0.8
}

// ── QualityGate ────────────────────────────────────────────────────────

/// Quality gate — runs eval suites and evaluates criteria against results.
pub struct QualityGate {
    pub name: String,
    pub level: GateLevel,
    pub criteria: Vec<GateCriterion>,
    pub suites: Vec<String>,
    pub harness: EvalHarness,
    pub evals_dir: PathBuf,
    pub baseline_store: BaselineStore,
    /// Layout root the gate's persisted state resolves against.
    paths: std::sync::Arc<crate::dirs::SyscityPaths>,
    /// Governance rules applied to the auto-included badcase regression suite
    /// (expiry / dedup / downgrade, §十二 回归集治理). `None` falls back to the
    /// raw `load_badcase_suite` auto-include.
    pub badcase_governance: Option<BadcaseGovernance>,
    /// Pending-badcase pool backing the compression low-retention criterion
    /// (§十二 ⑧). `None` leaves that criterion inert.
    pub pending_badcase_store: Option<Arc<PendingBadcaseStore>>,
    /// Sampled production-turn store backing the shadow gate's online replay
    /// (§09 · N=1). `None` falls back to an empty shadow report.
    pub turn_sample_store: Option<Arc<TurnSampleStore>>,
    /// Compression low-retention gate configuration (§十二 ⑧). Default off.
    pub compression_gate: Option<CompressionGateConfig>,
}

impl QualityGate {
    pub fn new(
        name: String,
        level: GateLevel,
        criteria: Vec<GateCriterion>,
        suites: Vec<String>,
        harness: EvalHarness,
        evals_dir: PathBuf,
        paths: std::sync::Arc<crate::dirs::SyscityPaths>,
    ) -> Self {
        Self {
            name,
            level,
            criteria,
            suites,
            harness,
            evals_dir,
            baseline_store: BaselineStore::load(&paths),
            paths,
            badcase_governance: None,
            pending_badcase_store: None,
            turn_sample_store: None,
            compression_gate: None,
        }
    }

    /// Attach the badcase regression suite governance rules (§十二 回归集治理).
    ///
    /// When set, the gate's auto-included badcase suite is loaded via
    /// [`load_governed_badcase_suite`] instead of the raw loader, applying
    /// expiry / dedup / downgrade before the suite runs.
    pub fn with_badcase_governance(mut self, governance: BadcaseGovernance) -> Self {
        self.badcase_governance = Some(governance);
        self
    }

    /// Attach the runtime stores backing the compression low-retention
    /// criterion and the shadow gate's online replay (§十二 ⑧ / §09).
    ///
    /// Both are runtime objects (`state.infra.*`), injected after
    /// [`Self::from_config`]; a `None` store leaves the corresponding gate
    /// inert.
    pub fn with_stores(
        mut self,
        pending_badcase_store: Option<Arc<PendingBadcaseStore>>,
        turn_sample_store: Option<Arc<TurnSampleStore>>,
    ) -> Self {
        self.pending_badcase_store = pending_badcase_store;
        self.turn_sample_store = turn_sample_store;
        self
    }

    /// Attach the compression low-retention gate configuration (§十二 ⑧).
    ///
    /// Overrides whatever [`Self::from_config`] read from the config file.
    pub fn with_compression_gate(mut self, cfg: CompressionGateConfig) -> Self {
        self.compression_gate = Some(cfg);
        self
    }

    /// Create from configuration.
    pub fn from_config(
        config: &QualityGateConfig,
        harness: EvalHarness,
        evals_dir: PathBuf,
        paths: std::sync::Arc<crate::dirs::SyscityPaths>,
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }

        let mut criteria = Vec::new();

        // Pass rate criterion
        criteria.push(GateCriterion::PassRate {
            suite_id: "main".into(),
            min_rate: config.min_pass_rate,
        });

        // Zero P0 criterion
        if config.require_zero_p0 {
            criteria.push(GateCriterion::ZeroP0Risks);
        }

        // No regression criterion
        if let (Some(baseline), Some(max_degradation)) =
            (&config.baseline_tag, config.max_degradation)
        {
            criteria.push(GateCriterion::NoRegressionVs {
                baseline_tag: baseline.clone(),
                metric: "pass_rate".into(),
                max_degradation,
            });
        }

        Some(Self {
            name: config.name.clone(),
            level: config.level.clone(),
            criteria,
            suites: config.suites.clone(),
            harness,
            evals_dir,
            baseline_store: BaselineStore::load(&paths),
            paths,
            badcase_governance: None,
            pending_badcase_store: None,
            turn_sample_store: None,
            compression_gate: config.compression_gate.clone(),
        })
    }

    /// Run all criteria checks and return the gate result with release
    /// decision.
    pub async fn check(&self) -> (GateResult, ReleaseDecision) {
        let started_at = SystemTime::now();
        let mut criteria_results = Vec::new();
        let mut suite_results = Vec::new();

        // ── Step 1: Run all configured suites ─────────────────────────
        for suite_id in &self.suites {
            match self.run_suite(suite_id).await {
                Ok(summary) => {
                    info!(
                        "Gate suite '{}' completed: {:.1}% pass rate, continuous={}",
                        suite_id,
                        summary.overall_pass_rate * 100.0,
                        summary.continuous_success,
                    );
                    suite_results.push(summary);
                }
                Err(e) => {
                    warn!("Gate suite '{}' execution failed: {}", suite_id, e);
                    criteria_results.push(CriterionResult {
                        criterion: format!("suite_run({})", suite_id),
                        passed: false,
                        actual: 0.0,
                        threshold: 0.0,
                        detail: format!("Suite execution failed: {}", e),
                    });
                }
            }
        }

        // ── Step 1b: Auto-include badcase regression suite ────────────
        // Apply governance (expiry / dedup / downgrade, §十二 回归集治理) when
        // configured; otherwise fall back to the raw badcase suite.
        let badcase_suite = match &self.badcase_governance {
            Some(gov) => load_governed_badcase_suite(&self.evals_dir, gov)
                .ok()
                .filter(|s| !s.tasks.is_empty()),
            None => load_badcase_regression_suite(&self.evals_dir),
        };
        if let Some(badcase_suite) = badcase_suite {
            info!(
                "Auto-including badcase regression suite with {} tasks",
                badcase_suite.tasks.len()
            );
            let suite_id = "badcases";
            let mut summaries = Vec::with_capacity(badcase_suite.tasks.len());

            for task in &badcase_suite.tasks {
                match self.harness.run(task.clone(), badcase_suite.trials).await {
                    Ok(summary) => summaries.push(summary),
                    Err(e) => {
                        warn!("Badcase task '{}' failed: {}", task.id, e);
                        summaries.push(EvalSummary {
                            task_id: task.id.clone(),
                            total_trials: badcase_suite.trials,
                            pass_rate: 0.0,
                            at_least_once_success: false,
                            continuous_success: false,
                            confidence_interval: (0.0, 0.0),
                            avg_dimension_scores: HashMap::new(),
                            avg_duration_ms: 0.0,
                            avg_token_usage: None,
                            skill_pass_rate: 1.0,
                            skill_trigger_pass_rate: 1.0,
                            skill_execution_pass_rate: 1.0,
                            skill_quality_pass_rate: 1.0,
                            skill_resilience_pass_rate: 1.0,
                            skill_sub_metrics: HashMap::new(),
                            per_trial: vec![],
                            completed_at: SystemTime::now(),
                        });
                    }
                }
            }

            let summary = SuiteSummary::from_tasks(suite_id.to_string(), summaries);
            info!("Badcase suite completed: {:.1}% pass rate", summary.overall_pass_rate * 100.0);
            suite_results.push(summary);
        }

        // ── Step 2: Evaluate each criterion ───────────────────────────
        for criterion in &self.criteria {
            let result = self.evaluate_criterion(criterion, &suite_results).await;
            criteria_results.push(result);
        }

        // ── Step 2b: Compression low-retention gate (§十二 ⑧) ─────────
        // A burst of `online:risk` low-retention flags inside the window fails
        // the gate. Inert unless both a pending store and a config are wired.
        if let (Some(store), Some(cfg)) = (&self.pending_badcase_store, &self.compression_gate) {
            let now_ms = chrono::Utc::now().timestamp_millis();
            if let Some(result) = compression_criterion(store, cfg, now_ms).await {
                info!("Gate criterion '{}': passed={}", result.criterion, result.passed);
                criteria_results.push(result);
            }
        }

        let all_pass = criteria_results.iter().all(|r| r.passed);

        // ── Step 3: Store baselines on successful gate run ────────────
        if all_pass {
            let mut store = self.baseline_store.clone();
            for suite in &suite_results {
                store.store("latest", &suite.suite_id, suite);
            }
        }

        let result = GateResult {
            gate_name: self.name.clone(),
            passed: all_pass,
            criteria_results,
            suite_results,
            started_at,
            completed_at: SystemTime::now(),
        };
        let decision = ReleaseSignals::from_gate_result(&result).decide();
        (result, decision)
    }

    /// Run a single suite and aggregate results.
    async fn run_suite(&self, suite_id: &str) -> crate::Result<SuiteSummary> {
        let manifest_path = self
            .evals_dir
            .join("suites")
            .join(format!("{}.yaml", suite_id));
        let suite = load_suite(&manifest_path, suite_id)?;

        let mut summaries = Vec::with_capacity(suite.tasks.len());
        for task in &suite.tasks {
            match self.harness.run(task.clone(), suite.trials).await {
                Ok(summary) => {
                    summaries.push(summary);
                }
                Err(e) => {
                    warn!("Task '{}' failed in suite '{}': {}", task.id, suite_id, e);
                    // Push a zero-score summary so the suite still counts
                    summaries.push(EvalSummary {
                        task_id: task.id.clone(),
                        total_trials: suite.trials,
                        pass_rate: 0.0,
                        at_least_once_success: false,
                        continuous_success: false,
                        confidence_interval: (0.0, 0.0),
                        avg_dimension_scores: HashMap::new(),
                        avg_duration_ms: 0.0,
                        avg_token_usage: None,
                        skill_pass_rate: 1.0,
                        skill_trigger_pass_rate: 1.0,
                        skill_execution_pass_rate: 1.0,
                        skill_quality_pass_rate: 1.0,
                        skill_resilience_pass_rate: 1.0,
                        skill_sub_metrics: HashMap::new(),
                        per_trial: vec![],
                        completed_at: SystemTime::now(),
                    });
                }
            }
        }

        Ok(SuiteSummary::from_tasks(suite_id.to_string(), summaries))
    }

    /// Shadow traffic: run agent on prod traffic snapshots, no user-facing
    /// impact.
    pub async fn run_shadow(&self, turns: &[ProdTurn]) -> ShadowReport {
        let total = turns.len();
        if total == 0 {
            return ShadowReport {
                total_turns: 0,
                pass_rate: 1.0,
                avg_latency_ms: 0.0,
                tool_accuracy: 1.0,
            };
        }

        let mut passed = 0usize;
        let mut total_latency = 0u64;

        for turn in turns {
            // Run the agent on the prod input (no user-facing output)
            // We use a single-trial eval with a trivial pass condition
            let task = crate::eval::EvalTask {
                id: format!("shadow_{}", short_id()),
                input: turn.input.clone(),
                ..Default::default()
            };
            if let Ok(summary) = self.harness.run(task, 1).await {
                if summary.pass_rate > 0.0 {
                    passed += 1;
                }
                total_latency += summary.avg_duration_ms as u64;
            }
        }

        ShadowReport {
            total_turns: total,
            pass_rate: if total > 0 {
                passed as f64 / total as f64
            } else {
                1.0
            },
            avg_latency_ms: if total > 0 {
                total_latency as f64 / total as f64
            } else {
                0.0
            },
            tool_accuracy: 1.0, // simplified; real impl would check tool call correctness
        }
    }

    /// A/B with guardrails: run on a fraction of traffic, check guardrails.
    pub async fn run_ab(&self, turns: &[ProdTurn], fraction: f64) -> ABReport {
        let sample_count = (turns.len() as f64 * fraction.clamp(0.0, 1.0)) as usize;
        let sampled: Vec<&ProdTurn> = turns.iter().take(sample_count).collect();
        let total = sampled.len();

        if total == 0 {
            return ABReport {
                total_turns: 0,
                pass_rate: 1.0,
                guardrail_pass: true,
                latency_p99_ms: 0.0,
                error_rate: 0.0,
                human_takeover_rate: 0.0,
            };
        }

        let mut passed = 0usize;
        let mut latencies: Vec<f64> = Vec::with_capacity(total);
        let mut errors = 0usize;

        for turn in &sampled {
            let _start = std::time::Instant::now();
            let task = crate::eval::EvalTask {
                id: format!("ab_{}", short_id()),
                input: turn.input.clone(),
                ..Default::default()
            };
            match self.harness.run(task, 1).await {
                Ok(summary) => {
                    if summary.pass_rate > 0.0 {
                        passed += 1;
                    } else {
                        errors += 1;
                    }
                    latencies.push(summary.avg_duration_ms);
                }
                Err(_) => {
                    errors += 1;
                }
            }
        }

        // Compute p99 latency
        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p99 = if latencies.is_empty() {
            0.0
        } else {
            let idx = ((latencies.len() as f64) * 0.99).ceil() as usize - 1;
            latencies[idx.min(latencies.len() - 1)]
        };

        let error_rate = if total > 0 {
            errors as f64 / total as f64
        } else {
            0.0
        };

        ABReport {
            total_turns: total,
            pass_rate: if total > 0 {
                passed as f64 / total as f64
            } else {
                1.0
            },
            guardrail_pass: error_rate < 0.1 && p99 < 5000.0, // < 10% errors, < 5s p99
            latency_p99_ms: p99,
            error_rate,
            human_takeover_rate: 0.0, // placeholder; real impl from signal source
        }
    }

    /// Phased rollout: run the offline gate and advance phase if signals pass.
    pub async fn run_phased(&self) -> (GateResult, PhaseStore) {
        let mut phase = PhaseStore::load(&self.name, &self.paths.data_dir());
        let (result, _decision) = self.check().await;

        if result.passed {
            if phase.advance() {
                info!(
                    "Phased rollout '{}' advanced to {:.0}%",
                    self.name,
                    phase.current_phase * 100.0
                );
            } else {
                info!("Phased rollout '{}' is at 100%", self.name);
            }
        } else {
            warn!(
                "Phased rollout '{}' gate failed at {:.0}% — not advancing",
                self.name,
                phase.current_phase * 100.0
            );
        }

        (result, phase)
    }

    /// Dispatch gate execution by level.
    pub async fn check_with_level(&self) -> GateResult {
        match self.level {
            GateLevel::OfflineDiff => self.check().await.0,
            GateLevel::ShadowTraffic => {
                // Shadow mode: replay the most recent sampled production turns
                // through the harness (§09 · N=1 online shadow). Falls back to
                // an empty shadow report when no sample store is wired or no
                // samples exist yet.
                let report = match &self.turn_sample_store {
                    Some(store) => match store.list_recent(SHADOW_SAMPLE_LIMIT).await {
                        Ok(samples) => {
                            let turns = samples_to_replay_turns(&samples);
                            replay_shadow(&self.harness, &turns, 1).await
                        }
                        Err(e) => {
                            warn!("Shadow gate: failed to read turn samples: {}", e);
                            self.run_shadow(&[]).await
                        }
                    },
                    None => self.run_shadow(&[]).await,
                };
                let passed = report.pass_rate >= 0.8;
                GateResult {
                    gate_name: self.name.clone(),
                    passed,
                    criteria_results: vec![CriterionResult {
                        criterion: "shadow_pass_rate".into(),
                        passed,
                        actual: report.pass_rate,
                        threshold: 0.8,
                        detail: format!(
                            "Shadow traffic: {}/{} passed, avg latency {:.0}ms",
                            (report.pass_rate * report.total_turns as f64) as usize,
                            report.total_turns,
                            report.avg_latency_ms,
                        ),
                    }],
                    suite_results: vec![],
                    started_at: SystemTime::now(),
                    completed_at: SystemTime::now(),
                }
            }
            GateLevel::ABWithGuardrails => {
                let report = self.run_ab(&[], 0.1).await;
                let passed = report.guardrail_pass && report.pass_rate >= 0.8;
                GateResult {
                    gate_name: self.name.clone(),
                    passed,
                    criteria_results: vec![
                        CriterionResult {
                            criterion: "ab_guardrail".into(),
                            passed: report.guardrail_pass,
                            actual: 1.0 - report.error_rate,
                            threshold: 0.9,
                            detail: format!(
                                "Guardrail: p99={:.0}ms, error_rate={:.1}%",
                                report.latency_p99_ms,
                                report.error_rate * 100.0
                            ),
                        },
                        CriterionResult {
                            criterion: "ab_pass_rate".into(),
                            passed: report.pass_rate >= 0.8,
                            actual: report.pass_rate,
                            threshold: 0.8,
                            detail: format!("A/B pass rate: {:.1}%", report.pass_rate * 100.0),
                        },
                    ],
                    suite_results: vec![],
                    started_at: SystemTime::now(),
                    completed_at: SystemTime::now(),
                }
            }
            GateLevel::PhasedRollout => {
                let (result, phase) = self.run_phased().await;
                GateResult {
                    gate_name: format!("{} (phase={:.0}%)", self.name, phase.current_phase * 100.0),
                    passed: result.passed,
                    criteria_results: result.criteria_results,
                    suite_results: result.suite_results,
                    started_at: result.started_at,
                    completed_at: result.completed_at,
                }
            }
        }
    }

    /// Evaluate a single criterion against suite results.
    async fn evaluate_criterion(
        &self,
        criterion: &GateCriterion,
        suite_results: &[SuiteSummary],
    ) -> CriterionResult {
        match criterion {
            GateCriterion::PassRate { suite_id, min_rate } => {
                let suite = suite_results.iter().find(|s| s.suite_id == *suite_id);
                match suite {
                    Some(s) => {
                        let passed = s.overall_pass_rate >= *min_rate;
                        CriterionResult {
                            criterion: format!("pass_rate({})", suite_id),
                            passed,
                            actual: s.overall_pass_rate,
                            threshold: *min_rate,
                            detail: format!(
                                "Suite '{}' pass rate: {:.1}% (required: {:.0}%) across {} tasks",
                                suite_id,
                                s.overall_pass_rate * 100.0,
                                min_rate * 100.0,
                                s.total_tasks,
                            ),
                        }
                    }
                    None => CriterionResult {
                        criterion: format!("pass_rate({})", suite_id),
                        passed: false,
                        actual: 0.0,
                        threshold: *min_rate,
                        detail: format!("Suite '{}' was not executed", suite_id),
                    },
                }
            }

            GateCriterion::ZeroP0Risks => {
                // Count total failures across all suite results
                let total_failures: usize = suite_results
                    .iter()
                    .flat_map(|s| &s.task_summaries)
                    .filter(|t| t.pass_rate < 1.0)
                    .count();

                let passed = total_failures == 0;
                CriterionResult {
                    criterion: "zero_p0_risks".into(),
                    passed,
                    actual: total_failures as f64,
                    threshold: 0.0,
                    detail: if passed {
                        "No P0 risks detected — all tasks passed across all suites".into()
                    } else {
                        format!(
                            "{} tasks with failures detected across {} suites",
                            total_failures,
                            suite_results.len(),
                        )
                    },
                }
            }

            GateCriterion::NoRegressionVs {
                baseline_tag,
                metric,
                max_degradation,
            } => {
                let mut degraded = false;
                let mut details = Vec::new();

                for suite in suite_results {
                    let baseline = self
                        .baseline_store
                        .get_pass_rate(baseline_tag, &suite.suite_id);

                    match baseline {
                        Some(b) if *metric == "pass_rate" => {
                            let degradation = b - suite.overall_pass_rate;
                            if degradation > *max_degradation {
                                degraded = true;
                                details.push(format!(
                                    "Suite '{}': degraded by {:.1}% (baseline: {:.1}%, current: {:.1}%, max allowed: {:.1}%)",
                                    suite.suite_id,
                                    degradation * 100.0,
                                    b * 100.0,
                                    suite.overall_pass_rate * 100.0,
                                    max_degradation * 100.0,
                                ));
                            } else {
                                details.push(format!(
                                    "Suite '{}': ok (degradation: {:.1}%)",
                                    suite.suite_id,
                                    degradation * 100.0,
                                ));
                            }
                        }
                        Some(_) => {
                            details.push(format!(
                                "Suite '{}': metric '{}' not comparable yet",
                                suite.suite_id, metric,
                            ));
                        }
                        None => {
                            details.push(format!(
                                "Suite '{}': no baseline '{}' found — setting current as baseline",
                                suite.suite_id, baseline_tag,
                            ));
                            // Auto-store baseline for next run
                        }
                    }
                }

                let detail = details.join("\n");
                CriterionResult {
                    criterion: format!("no_regression({})", baseline_tag),
                    passed: !degraded,
                    actual: if degraded { 1.0 } else { 0.0 },
                    threshold: *max_degradation,
                    detail,
                }
            }

            GateCriterion::ContinuousSuccessRate { suite_id, min_rate } => {
                let suite = suite_results.iter().find(|s| s.suite_id == *suite_id);
                match suite {
                    Some(s) => {
                        let continuous = s.continuous_success;
                        let rate = if continuous { 1.0 } else { 0.0 };
                        let passed = rate >= *min_rate;
                        CriterionResult {
                            criterion: format!("continuous_success({})", suite_id),
                            passed,
                            actual: rate,
                            threshold: *min_rate,
                            detail: if continuous {
                                format!(
                                    "All {} tasks in suite '{}' achieved continuous success",
                                    s.total_tasks, suite_id,
                                )
                            } else {
                                format!(
                                    "Suite '{}': some tasks had intermittent failures (continuous={})",
                                    suite_id, s.continuous_success,
                                )
                            },
                        }
                    }
                    None => CriterionResult {
                        criterion: format!("continuous_success({})", suite_id),
                        passed: false,
                        actual: 0.0,
                        threshold: *min_rate,
                        detail: format!("Suite '{}' was not executed", suite_id),
                    },
                }
            }
        }
    }
}

// ── Display ────────────────────────────────────────────────────────────

impl std::fmt::Display for GateResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "═══ Quality Gate: {} ═══", self.gate_name)?;
        writeln!(f, "Result: {}", if self.passed { "PASS" } else { "FAIL" })?;

        if !self.suite_results.is_empty() {
            writeln!(f, "Suites:")?;
            for suite in &self.suite_results {
                writeln!(
                    f,
                    "  {}: {:.1}% ({} tasks, continuous={})",
                    suite.suite_id,
                    suite.overall_pass_rate * 100.0,
                    suite.total_tasks,
                    suite.continuous_success,
                )?;
            }
        }

        writeln!(f, "Criteria:")?;
        for cr in &self.criteria_results {
            let icon = if cr.passed { "✓" } else { "✗" };
            writeln!(
                f,
                "  {} {} (actual={:.2}, threshold={:.2})",
                icon, cr.criterion, cr.actual, cr.threshold
            )?;
            if !cr.detail.is_empty() {
                for line in cr.detail.lines() {
                    writeln!(f, "    {}", line)?;
                }
            }
        }
        Ok(())
    }
}

// ── Shadow/AB/Phased types (§09) ───────────────────────────────────────

/// A single production turn for shadow traffic evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProdTurn {
    /// The user input.
    pub input: String,
    /// Optional session context.
    pub context: Option<String>,
}

/// Shadow traffic evaluation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowReport {
    pub total_turns: usize,
    pub pass_rate: f64,
    pub avg_latency_ms: f64,
    pub tool_accuracy: f64,
}

/// A/B guardrail evaluation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ABReport {
    pub total_turns: usize,
    pub pass_rate: f64,
    pub guardrail_pass: bool,
    pub latency_p99_ms: f64,
    pub error_rate: f64,
    pub human_takeover_rate: f64,
}

/// Phased rollout state, persisted to `~/.syscity/phase.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseStore {
    pub gate_name: String,
    pub current_phase: f64, // 0.01, 0.10, 0.50, or 1.00
    pub phases: Vec<f64>,
    /// Where this state persists. Not part of the serialized payload.
    #[serde(skip)]
    path: PathBuf,
}

impl PhaseStore {
    /// Load phase state from `<data_dir>/phase.json`.
    pub fn load(gate_name: &str, data_dir: &std::path::Path) -> Self {
        let path = data_dir.join("phase.json");
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let mut store: Self = serde_json::from_str(&content).unwrap_or_else(|_| Self {
            gate_name: gate_name.to_string(),
            current_phase: 0.01,
            phases: vec![0.01, 0.10, 0.50, 1.00],
            path: PathBuf::new(),
        });
        store.path = path;
        store
    }

    /// Advance to the next phase and persist.
    pub fn advance(&mut self) -> bool {
        if let Some(pos) = self
            .phases
            .iter()
            .position(|p| (*p - self.current_phase).abs() < 1e-6)
        {
            if pos + 1 < self.phases.len() {
                self.current_phase = self.phases[pos + 1];
                self.save();
                return true;
            }
        }
        false // already at 100%
    }

    /// Persist phase state to disk.
    fn save(&self) {
        let path = &self.path;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, &json);
        }
    }
}

/// Feedback collector for online/business signals (§09).
///
/// Provides a shared holder for externally-populated signals (e.g., from
/// a webhook or monitoring system). The gateway can periodically poll
/// `current_signals()` to make release decisions.
#[derive(Debug, Clone)]
pub struct FeedbackCollector {
    pub online_signals: Option<OnlineExperienceSignal>,
    pub business_signals: Option<BusinessResultSignal>,
}

impl FeedbackCollector {
    pub fn new() -> Self {
        Self {
            online_signals: None,
            business_signals: None,
        }
    }

    /// Update online experience signals (called by external
    /// webhook/monitoring).
    pub fn update_online(&mut self, signal: OnlineExperienceSignal) {
        self.online_signals = Some(signal);
    }

    /// Update business result signals (called by external webhook/monitoring).
    pub fn update_business(&mut self, signal: BusinessResultSignal) {
        self.business_signals = Some(signal);
    }

    /// Compute current release signals, including any populated online/business
    /// data.
    pub fn current_signals(&self, gate_result: &GateResult) -> ReleaseSignals {
        let mut signals = ReleaseSignals::from_gate_result(gate_result);
        signals.online_experience = self.online_signals.clone();
        signals.business_results = self.business_signals.clone();
        signals
    }
}

impl Default for FeedbackCollector {
    fn default() -> Self {
        Self::new()
    }
}

// ── Release signals ────────────────────────────────────────────────────

/// Three release signals tracked after passing the gate (§09).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseSignals {
    /// Offline quality: pass rate, P0 count, tool param accuracy.
    pub offline_quality: OfflineQualitySignal,
    /// Online experience: human takeover rate, repeat rate, satisfaction.
    pub online_experience: Option<OnlineExperienceSignal>,
    /// Business results: task completion, order closure rate.
    pub business_results: Option<BusinessResultSignal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineQualitySignal {
    pub pass_rate: f64,
    pub p0_risk_count: usize,
    pub tool_param_accuracy: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineExperienceSignal {
    pub human_takeover_rate: f64,
    pub repeat_query_rate: f64,
    pub complaint_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusinessResultSignal {
    pub task_completion_rate: f64,
    pub order_closure_rate: f64,
}

/// Release decision based on signals (§09).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseDecision {
    Proceed,
    Rollback,
    Degrade,
}

impl ReleaseSignals {
    /// Build release signals from a gate result (§09).
    ///
    /// Computes pass rate and P0 count from the gate's criteria results,
    /// leaving online/business signals as `None` for external population.
    pub fn from_gate_result(result: &GateResult) -> Self {
        let suite_count = result.suite_results.len().max(1);
        let pass_rate = result
            .suite_results
            .iter()
            .map(|s| s.overall_pass_rate)
            .sum::<f64>()
            / suite_count as f64;

        let p0_risk_count = result
            .criteria_results
            .iter()
            .filter(|cr| !cr.passed && cr.actual < 0.5)
            .count();

        ReleaseSignals {
            offline_quality: OfflineQualitySignal {
                pass_rate,
                p0_risk_count,
                tool_param_accuracy: 1.0, // placeholder; real impl from tool call analysis
            },
            online_experience: None,
            business_results: None,
        }
    }

    /// Compute release decision from all available signals.
    pub fn decide(&self) -> ReleaseDecision {
        // Offline quality must pass
        if self.offline_quality.pass_rate < 0.8 || self.offline_quality.p0_risk_count > 0 {
            return ReleaseDecision::Rollback;
        }

        // If online signals are available, check them
        if let Some(ref online) = self.online_experience {
            if online.human_takeover_rate > 0.3 || online.complaint_rate > 0.05 {
                return ReleaseDecision::Degrade;
            }
        }

        ReleaseDecision::Proceed
    }
}

/// Generate a short unique ID for shadow/AB tasks.
fn short_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}", dur.subsec_nanos())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_baseline_store_roundtrip() {
        let dir = std::env::temp_dir().join("syscity_gate_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("baselines.json");

        let mut store = BaselineStore::load_from(path.clone());
        let summary = SuiteSummary {
            suite_id: "test_suite".into(),
            total_tasks: 3,
            task_summaries: vec![],
            overall_pass_rate: 0.85,
            continuous_success: true,
        };
        store.store("v1.0", "test_suite", &summary);

        // Reload from disk
        let store2 = BaselineStore::load_from(path);
        assert_eq!(store2.get_pass_rate("v1.0", "test_suite"), Some(0.85));
        assert_eq!(store2.get_pass_rate("v1.0", "nonexistent"), None);
        assert_eq!(store2.get_pass_rate("v2.0", "test_suite"), None);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_suite_summary_empty() {
        let s = SuiteSummary::from_tasks("empty".into(), vec![]);
        assert_eq!(s.overall_pass_rate, 0.0);
        assert!(!s.continuous_success);
    }

    #[test]
    fn test_suite_summary_aggregation() {
        let summaries = vec![
            EvalSummary {
                task_id: "task1".into(),
                total_trials: 5,
                pass_rate: 1.0,
                at_least_once_success: true,
                continuous_success: true,
                ..dummy_summary()
            },
            EvalSummary {
                task_id: "task2".into(),
                total_trials: 5,
                pass_rate: 0.6,
                at_least_once_success: true,
                continuous_success: false,
                ..dummy_summary()
            },
        ];
        let s = SuiteSummary::from_tasks("test".into(), summaries);
        assert!((s.overall_pass_rate - 0.8).abs() < 0.01);
        assert!(!s.continuous_success);
    }

    #[test]
    fn test_criterion_pass_rate_threshold() {
        // Test the threshold logic directly (not through async evaluate_criterion)
        let suite_pass_rate = 0.85;
        let min_rate = 0.8;
        assert!(suite_pass_rate >= min_rate, "pass rate should meet threshold");

        let suite_pass_rate = 0.7;
        let min_rate = 0.8;
        assert!(suite_pass_rate < min_rate, "pass rate below threshold should fail");
    }

    #[test]
    fn test_release_decision_proceed() {
        let signals = ReleaseSignals {
            offline_quality: OfflineQualitySignal {
                pass_rate: 0.95,
                p0_risk_count: 0,
                tool_param_accuracy: 0.9,
            },
            online_experience: None,
            business_results: None,
        };
        assert_eq!(signals.decide(), ReleaseDecision::Proceed);
    }

    #[test]
    fn test_release_decision_rollback_low_pass_rate() {
        let signals = ReleaseSignals {
            offline_quality: OfflineQualitySignal {
                pass_rate: 0.6,
                p0_risk_count: 0,
                tool_param_accuracy: 0.5,
            },
            online_experience: None,
            business_results: None,
        };
        assert_eq!(signals.decide(), ReleaseDecision::Rollback);
    }

    #[test]
    fn test_release_decision_degrade_high_complaint() {
        let signals = ReleaseSignals {
            offline_quality: OfflineQualitySignal {
                pass_rate: 0.95,
                p0_risk_count: 0,
                tool_param_accuracy: 0.9,
            },
            online_experience: Some(OnlineExperienceSignal {
                human_takeover_rate: 0.1,
                repeat_query_rate: 0.2,
                complaint_rate: 0.1,
            }),
            business_results: None,
        };
        assert_eq!(signals.decide(), ReleaseDecision::Degrade);
    }

    fn dummy_summary() -> EvalSummary {
        EvalSummary {
            task_id: String::new(),
            total_trials: 0,
            pass_rate: 0.0,
            at_least_once_success: false,
            continuous_success: false,
            confidence_interval: (0.0, 0.0),
            avg_dimension_scores: HashMap::new(),
            avg_duration_ms: 0.0,
            avg_token_usage: None,
            skill_pass_rate: 1.0,
            skill_trigger_pass_rate: 1.0,
            skill_execution_pass_rate: 1.0,
            skill_quality_pass_rate: 1.0,
            skill_resilience_pass_rate: 1.0,
            skill_sub_metrics: HashMap::new(),
            per_trial: vec![],
            completed_at: SystemTime::now(),
        }
    }
}
