//! Badcase Recycling Pipeline — collect, analyze, and persist failed eval
//! trials as recyclable evaluation tasks (§05).
//!
//! # Pipeline
//!
//! 1. `BadcaseCollector::collect()` extracts failed trials from `EvalSummary`
//! 2. Determines a human-readable `failure_reason` from
//!    condition/critique/skill results
//! 3. Optionally runs `RcaPipeline` analysis for deep root-cause insight
//! 4. Persists as YAML to `evals/badcases/<task_id>.yaml` (append to existing)
//! 5. `load_badcase_suite()` re-loads collected badcases as a regression suite
//!
//! # Design
//!
//! - Collection happens **after** harness.run() — zero changes to EvalHarness.
//! - YAML output uses `serde_norway::Value` tree to match the `YamlTask`
//!   intermediate schema, avoiding `GoalCondition` → `YamlCondition` round-trip
//!   mismatch.
//! - RCA integration is optional (`Option<Arc<RcaPipeline>>`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::eval::dataset::{EvalSuite, EvalTask, EvalTaskSource, SuiteCategory};
use crate::eval::harness::{EvalSummary, TrialResult};
use crate::eval::loader::{default_evals_dir, load_tasks};
use crate::eval::rca::{
    rca_input_from_trial, BadcaseEntry, CandidateModule, ProblemPhenomenon, RcaPipeline, RcaResult,
};
use crate::goal::condition::GoalCondition;
use crate::Result;

// ── Types ───────────────────────────────────────────────────────────────

/// Lifecycle status of a badcase fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BadcaseFixStatus {
    /// Newly collected, not yet reviewed.
    Unconfirmed,
    /// Confirmed as a valid badcase.
    Confirmed,
    /// Fix in progress or applied.
    Fixed,
    /// Verified fixed via regression.
    Verified,
}

/// A single badcase record collected from a failed eval trial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadcaseRecord {
    /// Unique identifier — `{task_id}_{short_timestamp}`.
    pub id: String,
    /// Original eval task id.
    pub task_id: String,
    /// The user input that triggered the failure.
    pub input: String,
    /// Description from the original task.
    pub description: String,
    /// Human-readable failure reason.
    pub failure_reason: String,
    /// The agent's response.
    pub response: String,
    /// Whether RCA was run on this badcase.
    pub rca_performed: bool,
    /// RCA result (if run).
    pub rca_result: Option<RcaResult>,
    /// When this was collected.
    pub collected_at: SystemTime,
    /// Fix lifecycle status.
    pub fix_status: BadcaseFixStatus,
    /// Badcase entry source.
    pub entry: BadcaseEntry,
    /// Difficulty label for regression weighting (§八): `"easy"` | `"medium"`
    /// | `"hard"`. Defaults to `"medium"` so pre-existing badcase YAML still
    /// parses.
    #[serde(default = "default_badcase_difficulty")]
    pub difficulty: String,
    /// Coverage tags for regression attribution (§八), e.g. `tools`,
    /// `routing`, `prompt`, `retrieval`, `compression`. Defaults to empty.
    #[serde(default)]
    pub coverage: Vec<String>,
}

/// Default difficulty applied to badcases without an explicit label (§八).
fn default_badcase_difficulty() -> String {
    "medium".to_string()
}

/// A cluster of similar badcases grouped by phenomenon and module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadcaseCluster {
    /// The problem phenomenon (what the user sees).
    pub phenomenon: Option<ProblemPhenomenon>,
    /// The primary responsible module.
    pub primary_module: Option<CandidateModule>,
    /// Number of badcases in this cluster.
    pub count: usize,
    /// Task IDs of clustered badcases.
    pub task_ids: Vec<String>,
    /// Common failure reason summary.
    pub common_failure_reason: String,
}

/// Governance rules for badcase regression suite (§09).
///
/// Controls deduplication, expiry, and downgrade of recycled badcases
/// to prevent suite bloat and ensure signal quality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadcaseGovernance {
    /// Badcases older than this many days are excluded from the regression
    /// suite.
    pub max_age_days: u64,
    /// Maximum number of badcase records with the same input before dedup kicks
    /// in.
    pub max_duplicate_inputs: usize,
    /// After a task_id appears this many times across badcase files,
    /// its effective min_pass_rate is lowered to `downgraded_pass_rate`.
    pub downgrade_threshold: usize,
    /// The reduced pass rate applied to downgraded tasks.
    pub downgraded_pass_rate: f64,
    /// Difficulty label → trial-count multiplier (§八). Harder cases get
    /// proportionally more trials in the governed regression suite.
    #[serde(default)]
    pub difficulty_weights: HashMap<String, f64>,
    /// Multiplier applied when a badcase difficulty has no explicit entry.
    #[serde(default = "default_badcase_default_weight")]
    pub default_weight: f64,
}

/// Default multiplier for difficulties without an explicit weight (§八).
fn default_badcase_default_weight() -> f64 {
    1.0
}

impl Default for BadcaseGovernance {
    fn default() -> Self {
        Self {
            max_age_days: 90,
            max_duplicate_inputs: 3,
            downgrade_threshold: 10,
            downgraded_pass_rate: 0.7,
            difficulty_weights: HashMap::new(),
            default_weight: default_badcase_default_weight(),
        }
    }
}

impl BadcaseGovernance {
    /// Filter out expired records (older than `max_age_days`).
    pub fn filter_expired(&self, records: &[BadcaseRecord]) -> Vec<BadcaseRecord> {
        let cutoff = std::time::Duration::from_secs(self.max_age_days * 86400);
        let now = SystemTime::now();
        records
            .iter()
            .filter(|r| {
                now.duration_since(r.collected_at)
                    .map(|age| age < cutoff)
                    .unwrap_or(true) // if clock went backwards, keep it
            })
            .cloned()
            .collect()
    }

    /// Check if a new record is a duplicate (same `input` already exists too
    /// many times).
    pub fn is_duplicate(&self, input: &str, existing: &[BadcaseRecord]) -> bool {
        let count = existing.iter().filter(|r| r.input == input).count();
        count >= self.max_duplicate_inputs
    }

    /// Compute the effective min_pass_rate for a task, applying downgrade
    /// if the task has appeared in too many badcase records.
    pub fn effective_pass_rate(
        &self,
        task_id: &str,
        records: &[BadcaseRecord],
        default_min: f64,
    ) -> f64 {
        let count = records.iter().filter(|r| r.task_id == task_id).count();
        if count >= self.downgrade_threshold {
            self.downgraded_pass_rate.min(default_min)
        } else {
            default_min
        }
    }

    /// Resolve the trial-count multiplier for a difficulty label (§八).
    ///
    /// Returns the configured weight for `difficulty`, falling back to
    /// `default_weight` when the label has no explicit entry.
    pub fn weight_for(&self, difficulty: &str) -> f64 {
        self.difficulty_weights
            .get(difficulty)
            .copied()
            .unwrap_or(self.default_weight)
    }

    /// Compute the weighted trial count for a badcase task (§八).
    ///
    /// Scales `base_trials` by [`Self::weight_for`] and floors at 1 so a
    /// weight of zero never drops the task out of the suite entirely.
    pub fn weighted_trials(&self, difficulty: &str, base_trials: usize) -> usize {
        (base_trials as f64 * self.weight_for(difficulty))
            .round()
            .max(1.0) as usize
    }

    /// Build governance from the gateway config's badcase-governance section,
    /// merging the configured difficulty weights into the code defaults (§八).
    pub fn from_config(c: &crate::gateway::config::BadcaseGovernanceConfig) -> Self {
        Self {
            difficulty_weights: c.difficulty_weights.clone(),
            default_weight: c.default_weight,
            ..Self::default()
        }
    }
}

// ── Collector ───────────────────────────────────────────────────────────

/// Collects failed trials from an eval run and persists them as badcase YAML.
///
/// # Example
///
/// ```ignore
/// let collector = BadcaseCollector::new(None, None);
/// let n = collector.collect(&summary, &task).await?;
/// println!("Collected {} badcases", n);
/// ```
pub struct BadcaseCollector {
    /// Optional RCA pipeline for deep analysis.
    rca_pipeline: Option<Arc<RcaPipeline>>,
    /// Output directory for badcase YAML files (defaults to `evals/badcases/`).
    output_dir: PathBuf,
}

impl BadcaseCollector {
    /// Create a new badcase collector.
    ///
    /// * `rca_pipeline` — optional RCA pipeline for deep analysis of each
    ///   failure.
    /// * `output_dir` — directory for badcase YAML files (defaults to
    ///   `evals/badcases/`).
    pub fn new(rca_pipeline: Option<Arc<RcaPipeline>>, output_dir: Option<PathBuf>) -> Self {
        Self {
            rca_pipeline,
            output_dir: output_dir.unwrap_or_else(|| default_evals_dir().join("badcases")),
        }
    }

    /// Process all failed trials from a completed eval run.
    ///
    /// Returns the number of badcases collected (written to YAML).
    pub async fn collect(&self, summary: &EvalSummary, task: &EvalTask) -> Result<usize> {
        let failed_trials: Vec<&TrialResult> =
            summary.per_trial.iter().filter(|t| !t.passed).collect();

        if failed_trials.is_empty() {
            return Ok(0);
        }

        let mut count = 0usize;

        for trial in &failed_trials {
            info!("Collecting badcase: task={}, trial={}", task.id, trial.trial_index);

            // Determine failure reason from trial results
            let failure_reason = determine_failure_reason(trial);

            // Optionally run RCA
            let (rca_performed, rca_result) = if let Some(ref rca) = self.rca_pipeline {
                let input = rca_input_from_trial(&task.id, trial, &task.input);
                match rca.analyze(input).await {
                    Ok(result) => {
                        info!(
                            "RCA complete for task={}: {:?}",
                            task.id, result.responsibility_module
                        );
                        (true, Some(result))
                    }
                    Err(e) => {
                        warn!("RCA failed for task={}: {}", task.id, e);
                        (false, None)
                    }
                }
            } else {
                (false, None)
            };

            // Build record
            let record = BadcaseRecord {
                id: format!("{}_{}", task.id, short_timestamp()),
                task_id: task.id.clone(),
                input: task.input.clone(),
                description: task.description.clone(),
                failure_reason,
                response: trial.response.clone(),
                rca_performed,
                rca_result,
                collected_at: SystemTime::now(),
                fix_status: BadcaseFixStatus::Unconfirmed,
                entry: BadcaseEntry::AutoDetected,
                difficulty: task.difficulty.clone(),
                coverage: task.coverage.clone(),
            };

            // Persist as YAML
            write_badcase_yaml(&record, task, &self.output_dir)?;
            count += 1;
        }

        if count > 0 {
            info!("Collected {} badcases for task '{}'", count, task.id);
        }

        Ok(count)
    }

    /// Collect badcases with governance rules applied (§09).
    ///
    /// Same as `collect()`, but skips duplicates and expired records.
    pub async fn collect_and_govern(
        &self,
        summary: &EvalSummary,
        task: &EvalTask,
        governance: &BadcaseGovernance,
    ) -> Result<usize> {
        let failed_trials: Vec<&TrialResult> =
            summary.per_trial.iter().filter(|t| !t.passed).collect();

        if failed_trials.is_empty() {
            return Ok(0);
        }

        // Load existing records for dedup check
        // output_dir is evals/badcases/, so parent() is the evals directory
        let evals_dir = self
            .output_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(default_evals_dir);
        let existing_records = load_all_badcase_records(&evals_dir);

        let mut count = 0usize;

        for trial in &failed_trials {
            if governance.is_duplicate(&task.input, &existing_records) {
                info!(
                    "Skipping duplicate badcase for task='{}' (input already recorded {} times)",
                    task.id, governance.max_duplicate_inputs
                );
                continue;
            }

            info!("Collecting badcase: task={}, trial={}", task.id, trial.trial_index);
            let failure_reason = determine_failure_reason(trial);

            let (rca_performed, rca_result) = if let Some(ref rca) = self.rca_pipeline {
                let input = rca_input_from_trial(&task.id, trial, &task.input);
                match rca.analyze(input).await {
                    Ok(result) => (true, Some(result)),
                    Err(e) => {
                        warn!("RCA failed for task={}: {}", task.id, e);
                        (false, None)
                    }
                }
            } else {
                (false, None)
            };

            let record = BadcaseRecord {
                id: format!("{}_{}", task.id, short_timestamp()),
                task_id: task.id.clone(),
                input: task.input.clone(),
                description: task.description.clone(),
                failure_reason,
                response: trial.response.clone(),
                rca_performed,
                rca_result,
                collected_at: SystemTime::now(),
                fix_status: BadcaseFixStatus::Unconfirmed,
                entry: BadcaseEntry::AutoDetected,
                difficulty: task.difficulty.clone(),
                coverage: task.coverage.clone(),
            };

            write_badcase_yaml(&record, task, &self.output_dir)?;
            count += 1;
        }

        if count > 0 {
            info!("Collected {} badcases for task '{}' (governance applied)", count, task.id);
        }

        Ok(count)
    }
}

// ── Failure reason determination ────────────────────────────────────────

/// Derive a human-readable failure reason from trial result fields.
fn determine_failure_reason(trial: &TrialResult) -> String {
    let mut reasons = Vec::new();

    if !trial.conditions_passed {
        let failed: Vec<String> = trial
            .condition_results
            .iter()
            .filter(|r| !r.passed)
            .map(|r| {
                if r.detail.is_empty() {
                    format!("condition failed: actual '{}'", r.actual)
                } else {
                    format!("condition failed: {}", r.detail)
                }
            })
            .collect();
        if !failed.is_empty() {
            reasons.push(format!("conditions: {}", failed.join("; ")));
        }
    }

    if !trial.critique_passed {
        if let Some(ref c) = trial.critique {
            if !c.weaknesses.is_empty() {
                reasons.push(format!("critique: {}", c.weaknesses.join("; ")));
            } else {
                reasons.push("critique threshold not met".into());
            }
        } else {
            reasons.push("critique failed".into());
        }
    }

    if !trial.skill_passed {
        reasons.push("skill evaluation failed".into());
    }

    if !trial.session_conditions_passed {
        reasons.push("session conditions failed".into());
    }

    if reasons.is_empty() {
        "trial failed: unknown reason".into()
    } else {
        reasons.join("; ")
    }
}

// ── Clustering ──────────────────────────────────────────────────────────

/// Cluster badcase records by phenomenon and primary module.
///
/// Groups records that share the same phenomenon × module pair, making it
/// easy to identify systemic issues vs one-off failures.
#[allow(dead_code)]
pub fn cluster_badcases(records: &[BadcaseRecord]) -> Vec<BadcaseCluster> {
    #[derive(Hash, Eq, PartialEq, Clone)]
    struct ClusterKey {
        phenomenon: String,
        module: String,
    }

    let mut groups: HashMap<ClusterKey, Vec<&BadcaseRecord>> = HashMap::new();

    for record in records {
        let (phen, mod_) = if let Some(ref rca) = record.rca_result {
            (rca.phenomenon.clone(), format!("{:?}", rca.responsibility_module))
        } else {
            ("unknown".into(), "unknown".into())
        };
        let key = ClusterKey { phenomenon: phen, module: mod_ };
        groups.entry(key).or_default().push(record);
    }

    let mut clusters: Vec<BadcaseCluster> = groups
        .into_iter()
        .map(|(key, group)| {
            let task_ids: Vec<String> = group.iter().map(|r| r.task_id.clone()).collect();
            // Pick the most common failure reason
            let mut reason_counts: HashMap<&str, usize> = HashMap::new();
            for r in &group {
                *reason_counts.entry(&r.failure_reason).or_insert(0) += 1;
            }
            let common_reason = reason_counts
                .into_iter()
                .max_by_key(|&(_, count)| count)
                .map(|(reason, _)| reason.to_string())
                .unwrap_or_default();

            // Parse phenomenon and module from the cluster key
            let phenomenon = match key.phenomenon.as_str() {
                "NonResponsive" => Some(ProblemPhenomenon::NonResponsive),
                "OrderNotClarified" => Some(ProblemPhenomenon::OrderNotClarified),
                "FactualError" => Some(ProblemPhenomenon::FactualError),
                "OverPromise" => Some(ProblemPhenomenon::OverPromise),
                "ToolNotCalled" => Some(ProblemPhenomenon::ToolNotCalled),
                "ToolWrongOrder" => Some(ProblemPhenomenon::ToolWrongOrder),
                "Hallucination" => Some(ProblemPhenomenon::Hallucination),
                "RefusalError" => Some(ProblemPhenomenon::RefusalError),
                _ => None,
            };
            let primary_module = match key.module.as_str() {
                "IntentRecognition" => Some(CandidateModule::IntentRecognition),
                "SlotFilling" => Some(CandidateModule::SlotFilling),
                "ContextMemory" => Some(CandidateModule::ContextMemory),
                "Retrieval" => Some(CandidateModule::Retrieval),
                "ToolSelection" => Some(CandidateModule::ToolSelection),
                "ToolExecution" => Some(CandidateModule::ToolExecution),
                "ParameterConstruction" => Some(CandidateModule::ParameterConstruction),
                "Reasoning" => Some(CandidateModule::Reasoning),
                "ResponseGeneration" => Some(CandidateModule::ResponseGeneration),
                "PolicyEnforcement" => Some(CandidateModule::PolicyEnforcement),
                "SystemInfra" => Some(CandidateModule::SystemInfra),
                _ => None,
            };

            BadcaseCluster {
                phenomenon,
                primary_module,
                count: group.len(),
                task_ids,
                common_failure_reason: common_reason,
            }
        })
        .collect();

    // Sort by cluster size (largest first)
    clusters.sort_by_key(|c| std::cmp::Reverse(c.count));

    clusters
}

// ── YAML I/O ────────────────────────────────────────────────────────────

/// Persist a badcase record as YAML, appending to an existing file if present.
///
/// The output file at `{output_dir}/{task_id}.yaml` is a valid task YAML
/// parseable by `load_tasks()` — `source: badcase` is set on each entry.
pub fn write_badcase_yaml(
    record: &BadcaseRecord,
    original: &EvalTask,
    output_dir: &Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(output_dir)?;

    let file_path = output_dir.join(format!("{}.yaml", sanitize_id(&record.task_id)));

    // Load existing tasks if file exists
    let mut existing_tasks: Vec<serde_norway::Value> = if file_path.exists() {
        let content =
            std::fs::read_to_string(&file_path).map_err(crate::error::SyscityError::Io)?;
        let doc: serde_norway::Value =
            serde_norway::from_str(&content).unwrap_or(serde_norway::Value::Null);
        doc.get("tasks")
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Build the YAML mapping for this badcase task
    let task_yaml = build_badcase_yaml_task(record, original);
    existing_tasks.push(task_yaml);

    // Serialize to YAML
    let root = serde_norway::Value::Mapping({
        let mut m = serde_norway::Mapping::new();
        m.insert("tasks".to_string().into(), serde_norway::Value::Sequence(existing_tasks));
        m
    });

    let yaml_str = serde_norway::to_string(&root)
        .map_err(|e| crate::error::SyscityError::Validation(e.to_string()))?;
    std::fs::write(&file_path, yaml_str).map_err(crate::error::SyscityError::Io)?;

    info!("Badcase written to {:?}", file_path);
    Ok(file_path)
}

/// Build a `serde_norway::Value` mapping matching the `YamlTask` schema.
fn build_badcase_yaml_task(record: &BadcaseRecord, original: &EvalTask) -> serde_norway::Value {
    use serde_norway::Value;

    let mut task = serde_norway::Mapping::new();

    // id: append suffix to avoid collision with original task id
    task.insert(
        "id".to_string().into(),
        format!("{}_bc", &record.task_id[..record.task_id.len().min(50)]).into(),
    );

    // input
    task.insert("input".to_string().into(), record.input.clone().into());

    // description
    if !record.description.is_empty() {
        task.insert("description".to_string().into(), record.description.clone().into());
    }

    // expected_behavior
    if !original.expected_behavior.is_empty() {
        task.insert(
            "expected_behavior".to_string().into(),
            original.expected_behavior.clone().into(),
        );
    }

    // source: always "badcase"
    task.insert("source".to_string().into(), "badcase".into());

    // difficulty + coverage (§八) — round-trip the regression labels
    task.insert("difficulty".to_string().into(), record.difficulty.clone().into());
    if !record.coverage.is_empty() {
        task.insert("coverage".to_string().into(), record.coverage.clone().into());
    }

    // failure_reason
    task.insert("failure_reason".to_string().into(), record.failure_reason.clone().into());

    // rca_result (if available)
    if let Some(ref rca) = record.rca_result {
        if let Ok(val) = serde_norway::to_value(rca) {
            task.insert("rca_result".to_string().into(), val);
        }
    }

    // conditions (converted from GoalCondition to YamlCondition format)
    if !original.conditions.is_empty() {
        let conds: Vec<Value> = original
            .conditions
            .iter()
            .map(goal_condition_to_yaml)
            .collect();
        task.insert("conditions".to_string().into(), conds.into());
    }

    // criteria (if original has it)
    if let Some(ref criteria) = original.criteria {
        let mut crit = serde_norway::Mapping::new();
        let dims: Vec<Value> = criteria
            .dimensions
            .iter()
            .map(|d| format!("{:?}", d).into())
            .collect();
        crit.insert("dimensions".to_string().into(), dims.into());

        // thresholds: HashMap<String, f64> serializes cleanly
        let thresh_map: serde_norway::Mapping = criteria
            .thresholds
            .iter()
            .map(|(k, v)| (Value::String(k.clone()), Value::Number(serde_norway::Number::from(*v))))
            .collect();
        crit.insert("thresholds".to_string().into(), thresh_map.into());

        task.insert("criteria".to_string().into(), crit.into());
    }

    Value::Mapping(task)
}

/// Convert a `GoalCondition` to the `YamlCondition` intermediate format.
fn goal_condition_to_yaml(cond: &GoalCondition) -> serde_norway::Value {
    use serde_norway::Value;

    let mut m = serde_norway::Mapping::new();
    match cond {
        GoalCondition::ExitCode { command, expected } => {
            m.insert("type".to_string().into(), "exit_code".into());
            m.insert("command".to_string().into(), command.clone().into());
            if let Some(exp) = expected {
                m.insert("expected".to_string().into(), Value::Number((*exp).into()));
            }
        }
        GoalCondition::Pattern { command, must_contain } => {
            m.insert("type".to_string().into(), "pattern".into());
            m.insert("command".to_string().into(), command.clone().into());
            m.insert("must_contain".to_string().into(), must_contain.clone().into());
        }
        GoalCondition::FileExists { path } => {
            m.insert("type".to_string().into(), "file_exists".into());
            m.insert("path".to_string().into(), path.clone().into());
        }
        GoalCondition::Numeric { command, operator, threshold } => {
            m.insert("type".to_string().into(), "numeric".into());
            m.insert("command".to_string().into(), command.clone().into());
            m.insert("operator".to_string().into(), format!("{:?}", operator).into());
            m.insert(
                "threshold".to_string().into(),
                Value::Number(serde_norway::Number::from(*threshold)),
            );
        }
        GoalCondition::MustNotContain { command, must_not_contain } => {
            m.insert("type".to_string().into(), "must_not_contain".into());
            m.insert("command".to_string().into(), command.clone().into());
            m.insert("must_not_contain".to_string().into(), must_not_contain.clone().into());
        }
        GoalCondition::StaticAnalysis { command } => {
            m.insert("type".to_string().into(), "static_analysis".into());
            m.insert("command".to_string().into(), command.clone().into());
        }
    }
    Value::Mapping(m)
}

// ── Re-loading ──────────────────────────────────────────────────────────

/// Load all badcase YAML files from `evals/badcases/` as a regression suite.
///
/// Returns an empty suite (all rates = 1.0) when the directory doesn't exist
/// or contains no valid files — never fails for missing data.
pub fn load_badcase_suite(evals_dir: &Path) -> Result<EvalSuite> {
    let badcases_dir = evals_dir.join("badcases");

    if !badcases_dir.is_dir() {
        return Ok(EvalSuite {
            id: "badcases".into(),
            name: "Badcase Regression Suite".into(),
            category: SuiteCategory::Regression,
            tasks: Vec::new(),
            min_pass_rate: 1.0,
            trials: 3,
            continuous_success_required: false,
            sampling_rate: 1.0,
            tags: Vec::new(),
            agent_type: None,
            skill_designs: Vec::new(),
        });
    }

    let mut tasks = Vec::new();
    let mut read_dir = std::fs::read_dir(&badcases_dir).map_err(crate::error::SyscityError::Io)?;

    while let Some(entry) = read_dir.next().transpose()? {
        let path = entry.path();
        if path.extension().map(|e| e == "yaml").unwrap_or(false) {
            match load_tasks(&path) {
                Ok(loaded) => {
                    for mut t in loaded.tasks {
                        t.source = EvalTaskSource::BadcaseRecycle;
                        tasks.push(t);
                    }
                }
                Err(e) => {
                    warn!("Skipping badcase file {:?}: {}", path, e);
                }
            }
        }
    }

    info!("Loaded {} badcase tasks from {:?}", tasks.len(), badcases_dir);

    Ok(EvalSuite {
        id: "badcases".into(),
        name: "Badcase Regression Suite".into(),
        category: SuiteCategory::Regression,
        tasks,
        min_pass_rate: 1.0,
        trials: 3,
        continuous_success_required: false,
        sampling_rate: 1.0,
        tags: Vec::new(),
        agent_type: None,
        skill_designs: Vec::new(),
    })
}

/// Load all `BadcaseRecord`s from badcase YAML files.
///
/// Walks all YAML files in `{evals_dir}/badcases/`, parses each
/// `BadcaseRecord`. Returns an empty vec on any error or missing directory.
pub(crate) fn load_all_badcase_records(evals_dir: &Path) -> Vec<BadcaseRecord> {
    let badcases_dir = evals_dir.join("badcases");
    if !badcases_dir.is_dir() {
        return Vec::new();
    }

    let mut records = Vec::new();
    let read_dir = match std::fs::read_dir(&badcases_dir) {
        Ok(d) => d,
        Err(_) => return records,
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "yaml").unwrap_or(false) {
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let doc: serde_norway::Value = match serde_norway::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let tasks = match doc.get("tasks").and_then(|v| v.as_sequence()) {
                Some(t) => t,
                None => continue,
            };
            for task in tasks {
                if let Ok(r) = serde_norway::from_value::<BadcaseRecord>(task.clone()) {
                    records.push(r);
                }
            }
        }
    }
    records
}

/// Load the badcase regression suite with governance rules applied.
///
/// Filters expired records, applies downgraded pass rates to frequently
/// failing tasks. See `BadcaseGovernance` for rule details.
pub fn load_governed_badcase_suite(
    evals_dir: &Path,
    governance: &BadcaseGovernance,
) -> Result<EvalSuite> {
    let mut suite = load_badcase_suite(evals_dir)?;

    // ── Apply expiry ──
    let all_records = load_all_badcase_records(evals_dir);
    let active_records = governance.filter_expired(&all_records);

    if active_records.len() < all_records.len() {
        let expired = all_records.len() - active_records.len();
        info!("Filtered {} expired badcase records", expired);
    }

    // ── Apply downgrade ──
    let downgraded_min =
        governance.effective_pass_rate("__suite__", &active_records, suite.min_pass_rate);
    suite.min_pass_rate = downgraded_min;

    // ── Apply difficulty weighting (§八) ──
    // Harder badcases get proportionally more trials in the governed
    // regression suite via `weighted_trials`. The base is the suite's trial
    // count; the weighted per-task count is recorded on the task so the
    // harness can honor it (currently consumed at the suite level only).
    let base_trials = suite.trials;
    for task in &mut suite.tasks {
        let difficulty = task.difficulty.as_str();
        let weighted = governance.weighted_trials(difficulty, base_trials);
        task.trials = Some(weighted);
        if !task.coverage.is_empty() {
            debug!(
                "Badcase task '{}' weighted into suite: difficulty={}, trials={}->{}, coverage={:?}",
                task.id, difficulty, base_trials, weighted, task.coverage
            );
        }
    }

    Ok(suite)
}

/// Extract all `RcaResult` entries from badcase YAML files in
/// `{evals_dir}/badcases/`.
///
/// Walks each YAML file, parses `rca_result` keys, and collects non-None
/// results. Returns an empty vec if the directory doesn't exist or no RCA data
/// is found.
pub fn extract_rca_results_from_badcases(evals_dir: &Path) -> Result<Vec<RcaResult>> {
    let badcases_dir = evals_dir.join("badcases");

    if !badcases_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    let mut read_dir = std::fs::read_dir(&badcases_dir).map_err(crate::error::SyscityError::Io)?;

    while let Some(entry) = read_dir.next().transpose()? {
        let path = entry.path();
        if path.extension().map(|e| e == "yaml").unwrap_or(false) {
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read badcase file {:?}: {}", path, e);
                    continue;
                }
            };

            let doc: serde_norway::Value = match serde_norway::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Failed to parse badcase file {:?}: {}", path, e);
                    continue;
                }
            };

            let tasks = match doc.get("tasks").and_then(|v| v.as_sequence()) {
                Some(t) => t,
                None => continue,
            };

            for task in tasks {
                if let Some(rca_val) = task.get("rca_result") {
                    match serde_norway::from_value::<RcaResult>(rca_val.clone()) {
                        Ok(rca) => results.push(rca),
                        Err(e) => {
                            warn!("Failed to deserialize rca_result in {:?}: {}", path, e);
                        }
                    }
                }
            }
        }
    }

    info!(
        "Extracted {} RCA results from badcase files in {:?}",
        results.len(),
        badcases_dir
    );
    Ok(results)
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Generate a short timestamp string for unique IDs.
fn short_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}", dur.as_secs())
}

/// Sanitize a task ID for use as a filename.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::TrialResult;
    use crate::eval::rca::CandidateModule;
    use crate::goal::condition::Comparison;

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("hello_world"), "hello_world");
        assert_eq!(sanitize_id("test/123"), "test_123");
        assert_eq!(sanitize_id("___abc___"), "abc");
    }

    #[test]
    fn test_short_timestamp_not_empty() {
        let ts = short_timestamp();
        assert!(!ts.is_empty());
        assert!(ts.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_determine_failure_reason_unknown() {
        // A trial with no clear failure indicators
        let trial = TrialResult {
            trial_index: 0,
            response: "hello".into(),
            tool_calls: vec![],
            token_usage: None,
            duration_ms: 0,
            condition_results: vec![],
            conditions_passed: true,
            critique: None,
            critique_passed: true,
            skill_results: None,
            skill_passed: true,
            turn_results: vec![],
            session_condition_results: vec![],
            session_conditions_passed: true,
            passed: false,
        };
        let reason = determine_failure_reason(&trial);
        assert!(reason.contains("unknown"), "got: {}", reason);
    }

    #[test]
    fn test_goal_condition_to_yaml_roundtrip() {
        use crate::goal::condition::Comparison;

        let v = |s: &str| serde_norway::Value::String(s.to_string());

        let cond = GoalCondition::Pattern {
            command: "grep -c 'web_search' ${trial_dir}/trace.log".into(),
            must_contain: "1".into(),
        };
        let yaml_val = goal_condition_to_yaml(&cond);
        let mapping = yaml_val.as_mapping().unwrap();
        assert_eq!(mapping["type"], v("pattern"));
        assert_eq!(mapping["command"], v("grep -c 'web_search' ${trial_dir}/trace.log"));
        assert_eq!(mapping["must_contain"], v("1"));

        // Numeric variant
        let cond2 = GoalCondition::Numeric {
            command: "wc -l output.txt".into(),
            operator: Comparison::Ge,
            threshold: 3.0,
        };
        let yaml_val2 = goal_condition_to_yaml(&cond2);
        let m2 = yaml_val2.as_mapping().unwrap();
        assert_eq!(m2["type"], v("numeric"));
        assert_eq!(m2["operator"], v("Ge"));
        assert_eq!(m2["threshold"].as_f64().unwrap(), 3.0);

        // ExitCode variant
        let cond3 = GoalCondition::ExitCode {
            command: "ls /tmp".into(),
            expected: Some(0),
        };
        let yaml_val3 = goal_condition_to_yaml(&cond3);
        let m3 = yaml_val3.as_mapping().unwrap();
        assert_eq!(m3["type"], v("exit_code"));
        assert_eq!(m3["expected"].as_i64().unwrap(), 0);

        // FileExists variant
        let cond4 = GoalCondition::FileExists {
            path: "/tmp/result.json".into(),
        };
        let yaml_val4 = goal_condition_to_yaml(&cond4);
        let m4 = yaml_val4.as_mapping().unwrap();
        assert_eq!(m4["type"], v("file_exists"));
        assert_eq!(m4["path"], v("/tmp/result.json"));
    }

    #[test]
    fn test_load_badcase_suite_no_dir() {
        let suite = load_badcase_suite(Path::new("/nonexistent/evals")).unwrap();
        assert_eq!(suite.id, "badcases");
        assert!(suite.tasks.is_empty());
    }

    #[test]
    fn test_cluster_badcases_empty() {
        let clusters = cluster_badcases(&[]);
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_cluster_badcases_no_rca() {
        let records = vec![
            BadcaseRecord {
                id: "test_1".into(),
                task_id: "task_a".into(),
                input: "hello".into(),
                description: String::new(),
                failure_reason: "condition failed".into(),
                response: "response".into(),
                rca_performed: false,
                rca_result: None,
                collected_at: SystemTime::now(),
                fix_status: BadcaseFixStatus::Unconfirmed,
                entry: BadcaseEntry::AutoDetected,
                difficulty: "medium".into(),
                coverage: Vec::new(),
            },
            BadcaseRecord {
                id: "test_2".into(),
                task_id: "task_b".into(),
                input: "world".into(),
                description: String::new(),
                failure_reason: "critique failed".into(),
                response: "response".into(),
                rca_performed: false,
                rca_result: None,
                collected_at: SystemTime::now(),
                fix_status: BadcaseFixStatus::Unconfirmed,
                entry: BadcaseEntry::AutoDetected,
                difficulty: "medium".into(),
                coverage: Vec::new(),
            },
        ];
        // Without RCA results, all cluster as "unknown/unknown"
        let clusters = cluster_badcases(&records);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 2);
    }

    #[test]
    fn test_cluster_by_size_descending() {
        // Records with RCA results to test proper clustering
        let make_rca = |phenomenon: &str, module: CandidateModule| -> RcaResult {
            RcaResult {
                phenomenon: phenomenon.into(),
                process_deviation: "test".into(),
                responsibility: format!("{:?}", module),
                problem_category: "category".into(),
                problem_enumeration: "enum".into(),
                responsibility_module: module,
                sub_responsibility: None,
                evidence_chain: vec![],
                fix_suggestion: "fix".into(),
                confidence: 0.8,
                analysis_duration_ms: 0,
                entry: BadcaseEntry::AutoDetected,
                completed_at: SystemTime::now(),
            }
        };

        let records = vec![
            BadcaseRecord {
                id: "r1".into(),
                task_id: "t1".into(),
                input: "".into(),
                description: "".into(),
                failure_reason: "cond fail".into(),
                response: "".into(),
                rca_performed: true,
                rca_result: Some(make_rca("FactualError", CandidateModule::Retrieval)),
                collected_at: SystemTime::now(),
                fix_status: BadcaseFixStatus::Unconfirmed,
                entry: BadcaseEntry::AutoDetected,
                difficulty: "medium".into(),
                coverage: Vec::new(),
            },
            BadcaseRecord {
                id: "r2".into(),
                task_id: "t2".into(),
                input: "".into(),
                description: "".into(),
                failure_reason: "cond fail".into(),
                response: "".into(),
                rca_performed: true,
                rca_result: Some(make_rca("FactualError", CandidateModule::Retrieval)),
                collected_at: SystemTime::now(),
                fix_status: BadcaseFixStatus::Unconfirmed,
                entry: BadcaseEntry::AutoDetected,
                difficulty: "medium".into(),
                coverage: Vec::new(),
            },
            BadcaseRecord {
                id: "r3".into(),
                task_id: "t3".into(),
                input: "".into(),
                description: "".into(),
                failure_reason: "tool not called".into(),
                response: "".into(),
                rca_performed: true,
                rca_result: Some(make_rca("ToolNotCalled", CandidateModule::ToolSelection)),
                collected_at: SystemTime::now(),
                fix_status: BadcaseFixStatus::Unconfirmed,
                entry: BadcaseEntry::AutoDetected,
                difficulty: "medium".into(),
                coverage: Vec::new(),
            },
        ];

        let clusters = cluster_badcases(&records);
        // First cluster should be the largest (2 records)
        assert_eq!(clusters[0].count, 2);
        assert_eq!(clusters[1].count, 1);
    }

    #[test]
    fn test_governance_defaults() {
        let g = BadcaseGovernance::default();
        assert_eq!(g.max_age_days, 90);
        assert_eq!(g.max_duplicate_inputs, 3);
        assert_eq!(g.downgrade_threshold, 10);
        assert!((g.downgraded_pass_rate - 0.7).abs() < 1e-6);
    }

    #[test]
    fn test_governance_filter_expired() {
        let g = BadcaseGovernance {
            max_age_days: 1, // 1 day
            ..Default::default()
        };

        // Record from 2 days ago should be expired
        let two_days_ago = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(2 * 86400))
            .unwrap();
        let now_record = BadcaseRecord {
            id: "fresh".into(),
            task_id: "t1".into(),
            input: "hi".into(),
            description: "".into(),
            failure_reason: "fail".into(),
            response: "".into(),
            rca_performed: false,
            rca_result: None,
            collected_at: SystemTime::now(),
            fix_status: BadcaseFixStatus::Unconfirmed,
            entry: BadcaseEntry::AutoDetected,
            difficulty: "medium".into(),
            coverage: Vec::new(),
        };
        let old_record = BadcaseRecord {
            id: "stale".into(),
            collected_at: two_days_ago,
            ..now_record.clone()
        };

        let filtered = g.filter_expired(&[now_record.clone(), old_record]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "fresh");
    }

    #[test]
    fn test_governance_is_duplicate() {
        let g = BadcaseGovernance {
            max_duplicate_inputs: 2,
            ..Default::default()
        };

        let records = vec![
            BadcaseRecord {
                id: "r1".into(),
                input: "hello".into(),
                ..make_record()
            },
            BadcaseRecord {
                id: "r2".into(),
                input: "hello".into(),
                ..make_record()
            },
        ];

        assert!(g.is_duplicate("hello", &records));
        assert!(!g.is_duplicate("world", &records));
    }

    #[test]
    fn test_governance_effective_pass_rate() {
        let g = BadcaseGovernance {
            downgrade_threshold: 3,
            downgraded_pass_rate: 0.5,
            ..Default::default()
        };

        let records = vec![
            BadcaseRecord {
                id: "r1".into(),
                task_id: "frequent_fail".into(),
                ..make_record()
            },
            BadcaseRecord {
                id: "r2".into(),
                task_id: "frequent_fail".into(),
                ..make_record()
            },
            BadcaseRecord {
                id: "r3".into(),
                task_id: "frequent_fail".into(),
                ..make_record()
            },
        ];

        let rate = g.effective_pass_rate("frequent_fail", &records, 1.0);
        assert!((rate - 0.5).abs() < 1e-6);

        let rate_normal = g.effective_pass_rate("rare_fail", &records, 0.9);
        assert!((rate_normal - 0.9).abs() < 1e-6);
    }

    #[test]
    fn test_weight_for_maps_difficulty() {
        let mut weights = HashMap::new();
        weights.insert("hard".to_string().into(), 2.0);
        weights.insert("medium".to_string().into(), 1.5);
        weights.insert("easy".to_string().into(), 0.5);
        let g = BadcaseGovernance {
            difficulty_weights: weights,
            default_weight: 1.0,
            ..Default::default()
        };

        assert!((g.weight_for("hard") - 2.0).abs() < 1e-6);
        assert!((g.weight_for("medium") - 1.5).abs() < 1e-6);
        assert!((g.weight_for("easy") - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_weight_for_unknown_uses_default_weight() {
        let g = BadcaseGovernance {
            difficulty_weights: HashMap::new(),
            default_weight: 1.0,
            ..Default::default()
        };
        // Unknown label falls back to default_weight.
        assert!((g.weight_for("hard") - 1.0).abs() < 1e-6);
        assert!((g.weight_for("totally_unknown") - 1.0).abs() < 1e-6);

        // Default governance has no weights → everything maps to 1.0.
        let g = BadcaseGovernance::default();
        assert!((g.weight_for("hard") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_weighted_trials_scales() {
        let mut weights = HashMap::new();
        weights.insert("hard".to_string().into(), 2.0);
        let g = BadcaseGovernance {
            difficulty_weights: weights,
            default_weight: 1.0,
            ..Default::default()
        };

        // 2 base trials * weight 2.0 → 4.
        assert_eq!(g.weighted_trials("hard", 2), 4);
        // 3 base trials * weight 2.0 → 6.
        assert_eq!(g.weighted_trials("hard", 3), 6);
        // Unknown difficulty keeps the base trial count.
        assert_eq!(g.weighted_trials("medium", 3), 3);
    }

    #[test]
    fn test_weighted_trials_floors_at_one() {
        let mut weights = HashMap::new();
        weights.insert("easy".to_string().into(), 0.0);
        let g = BadcaseGovernance {
            difficulty_weights: weights,
            default_weight: 0.0,
            ..Default::default()
        };

        // A zero weight must not drop the task out of the suite entirely.
        assert_eq!(g.weighted_trials("easy", 5), 1);
    }

    #[test]
    fn test_from_config_merges_weights() {
        use crate::gateway::config::BadcaseGovernanceConfig;

        let mut weights = HashMap::new();
        weights.insert("hard".to_string().into(), 3.0);
        let cfg = BadcaseGovernanceConfig {
            difficulty_weights: weights,
            default_weight: 1.5,
        };

        let g = BadcaseGovernance::from_config(&cfg);

        // Config weights merged into the default governance values.
        assert_eq!(g.max_age_days, 90);
        assert!((g.weight_for("hard") - 3.0).abs() < 1e-6);
        assert!((g.default_weight - 1.5).abs() < 1e-6);
        assert!((g.weight_for("medium") - 1.5).abs() < 1e-6);

        // Empty config keeps default governance behaviour.
        let g = BadcaseGovernance::from_config(&BadcaseGovernanceConfig::default());
        assert!((g.weight_for("hard") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_badcase_record_default_labels() {
        // Old badcase YAML entries lack difficulty/coverage — they must parse
        // with the defaults (`medium`, empty coverage) so the regression
        // suite keeps loading pre-existing files.
        let yaml = r#"
            id: legacy_bc
            task_id: legacy_task
            input: hello
            description: legacy
            failure_reason: crit
            response: resp
            rca_performed: false
            fix_status: Unconfirmed
            entry: AutoDetected
            collected_at:
              secs_since_epoch: 1784000000
              nanos_since_epoch: 0
        "#;
        let record: BadcaseRecord = serde_norway::from_str(yaml).unwrap();
        assert_eq!(record.difficulty, "medium");
        assert!(record.coverage.is_empty());

        // Round-trip: the YAML writer emits the labels so they survive the
        // write → read cycle.
        let mut labeled = make_record();
        labeled.difficulty = "hard".into();
        labeled.coverage = vec!["tools".into(), "routing".into()];
        let value = build_badcase_yaml_task(&labeled, &crate::eval::EvalTask::default());
        let mapping = value.as_mapping().unwrap();
        assert_eq!(mapping.get("difficulty"), Some(&serde_norway::Value::String("hard".into())));
        let coverage = mapping.get("coverage").expect("coverage key");
        assert_eq!(
            coverage
                .as_sequence()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>(),
            vec!["tools", "routing"]
        );
    }

    #[test]
    fn test_eval_task_default_labels() {
        // Old task YAML without the new fields deserializes with defaults.
        let yaml = r#"
            id: legacy
            input: hi
        "#;
        let task: crate::eval::EvalTask = serde_norway::from_str(yaml).unwrap();
        assert_eq!(task.difficulty, "medium");
        assert!(task.coverage.is_empty());
        assert!(task.trials.is_none());
    }

    /// Helper: minimal BadcaseRecord for tests.
    fn make_record() -> BadcaseRecord {
        BadcaseRecord {
            id: String::new(),
            task_id: String::new(),
            input: String::new(),
            description: String::new(),
            failure_reason: String::new(),
            response: String::new(),
            rca_performed: false,
            rca_result: None,
            collected_at: SystemTime::now(),
            fix_status: BadcaseFixStatus::Unconfirmed,
            entry: BadcaseEntry::AutoDetected,
            difficulty: "medium".into(),
            coverage: Vec::new(),
        }
    }
}
