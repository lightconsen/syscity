//! YAML loader — reads `evals/*.yaml` into [`EvalTask`] / [`EvalSuite`].
//!
//! Handles two YAML formats:
//!
//! - **Task file** (`capability/*.yaml`, `regression/*.yaml`, …): a flat list
//!   of tasks with `id`, `input`, `conditions`, `criteria`, etc.
//! - **Suite manifest** (`suites/*.yaml`): orchestrates multiple task files
//!   with pass-rate thresholds, trial counts, and optional includes.
//!
//! # Limitations / format quirks
//!
//! - YAML `must_contain` can be integer or string — the loader converts both to
//!   `String` (required by `GoalCondition::Pattern`).
//! - Setup/cleanup commands are run by the harness before/after each trial.
//! - Suite manifests support three forms: flat `tasks:`, category-grouped
//!   (`capability: { suites: … }`), and `includes:` that reference other
//!   manifests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::warn;

use crate::agent::reflection::types::{QualityCriteria, QualityDimension};
use crate::eval::agent_type::AgentType;
use crate::eval::dataset::{
    EvalSuite, EvalTask, EvalTaskSource, SetupCommand, SkillEvalDesign, SuiteCategory, TurnInput,
};
use crate::goal::condition::GoalCondition;
use crate::Result;

// ═══════════════════════════════════════════════════════════════════════════
// Intermediate types — mirror the YAML schema exactly
// ═══════════════════════════════════════════════════════════════════════════

/// Top-level structure of a task YAML file (capability/*.yaml, etc.).
#[derive(Debug, Deserialize)]
struct YamlTaskFile {
    tasks: Vec<YamlTask>,
    /// Optional skill evaluation design (§02 / §04).
    #[serde(default)]
    skill_eval_design: Option<SkillEvalDesign>,
}

/// A single task in a task YAML file.
#[derive(Debug, Deserialize)]
struct YamlTask {
    id: String,
    #[serde(default)]
    description: String,
    input: String,
    #[serde(default)]
    expected_behavior: String,
    #[serde(default)]
    conditions: Vec<YamlCondition>,
    #[serde(default)]
    criteria: Option<YamlCriteria>,
    #[serde(default)]
    setup: Vec<YamlCommand>,
    #[serde(default)]
    cleanup: Vec<YamlCommand>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    failure_reason: Option<String>,
    /// Optional agent type for type-specific scoring emphasis (§02).
    #[serde(default)]
    agent_type: Option<String>,
    /// Multi-turn input sequence (§03).
    #[serde(default)]
    turns: Vec<YamlTurnInput>,
    /// Session-level conditions (§03).
    #[serde(default)]
    session_conditions: Vec<YamlCondition>,
    /// Difficulty label for regression weighting (§八): `"easy"` |
    /// `"medium"` | `"hard"`. Defaults to `"medium"` for old YAML.
    #[serde(default = "crate::eval::dataset::default_task_difficulty")]
    difficulty: String,
    /// Coverage tags for regression attribution (§八).
    #[serde(default)]
    coverage: Vec<String>,
}

/// A condition entry within a task.
#[derive(Debug, Deserialize)]
struct YamlCondition {
    #[serde(rename = "type")]
    cond_type: String,
    #[serde(default)]
    command: String,
    /// `must_contain` can be integer or string in YAML — we deserialise as
    /// raw `Value` and convert to `String` during conversion.
    #[serde(default)]
    must_contain: Option<serde_norway::Value>,
    #[serde(default)]
    expected: Option<i32>,
    #[serde(default)]
    path: String,
    #[serde(default)]
    operator: String,
    #[serde(default)]
    threshold: Option<f64>,
}

/// Criteria block — dimensions + per-dimension thresholds.
#[derive(Debug, Deserialize)]
struct YamlCriteria {
    #[serde(default)]
    dimensions: Vec<String>,
    #[serde(default)]
    thresholds: HashMap<String, f64>,
}

/// A setup/cleanup shell command.
#[derive(Debug, Deserialize)]
struct YamlCommand {
    command: String,
}

/// A single turn in a multi-turn task YAML (§03).
#[derive(Debug, Deserialize)]
struct YamlTurnInput {
    user_message: String,
    #[serde(default)]
    conditions: Vec<YamlCondition>,
}

// ── Suite-manifest intermediate types ───────────────────────────────────

/// A single task reference inside a flat suite manifest (ci_smoke.yaml).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct YamlTaskRef {
    id: String,
    path: String,
    #[serde(default)]
    task_filter: Option<String>,
    #[serde(default)]
    min_pass_rate: Option<f64>,
    #[serde(default)]
    trials: Option<usize>,
    #[serde(default)]
    sampling_rate: Option<f64>,
}

/// A suite entry inside a category (registry.yaml `capability.suites`).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct YamlSuiteEntry {
    id: String,
    path: String,
    #[serde(default)]
    weight: Option<f64>,
    #[serde(default)]
    min_pass_rate: Option<f64>,
    #[serde(default)]
    trials: Option<usize>,
    #[serde(default)]
    sampling_rate: Option<f64>,
    #[serde(default)]
    continuous_success_required: Option<bool>,
    #[serde(default)]
    task_filter: Option<String>,
}

/// A category in the registry-style manifest (registry.yaml `capability:`).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct YamlCategory {
    #[serde(default)]
    name: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    suites: Vec<YamlSuiteEntry>,
}

/// An include directive (release_gate.yaml).
#[derive(Debug, Deserialize)]
struct YamlInclude {
    path: String,
    #[serde(default)]
    sections: Vec<String>,
}

/// Top-level structure of a suite manifest YAML file.
///
/// This is a flexible type that can hold **one** of three shapes:
/// - flat `tasks:` (ci_smoke.yaml)
/// - category keys (registry.yaml)
/// - `includes:` (release_gate.yaml)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct YamlManifest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: String,
    /// Optional informational category label (e.g. "regression"). Without this
    /// field the flatten map below would try to parse it as a YamlCategory.
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    trials: Option<usize>,
    #[serde(default)]
    min_pass_rate: Option<f64>,
    #[serde(default)]
    continuous_success_required: Option<bool>,
    #[serde(default)]
    sampling_rate: Option<f64>,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    tasks: Vec<YamlTaskRef>,
    #[serde(flatten)]
    categories: HashMap<String, YamlCategory>,
    #[serde(default)]
    includes: Vec<YamlInclude>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Public API
// ═══════════════════════════════════════════════════════════════════════════

/// Determine the project root (`CARGO_MANIFEST_DIR` env or `cwd`).
fn project_root() -> PathBuf {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default())
}

/// Default `evals/` directory path.
pub fn default_evals_dir() -> PathBuf {
    project_root().join("evals")
}

/// List all available suites in the `suites/` directory.
///
/// Returns `(suite_stem, display_name)` pairs.
pub fn list_suites(evals_dir: &Path) -> Result<Vec<(String, String)>> {
    let suites_dir = evals_dir.join("suites");
    if !suites_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut suites = Vec::new();
    for entry in std::fs::read_dir(&suites_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "yaml").unwrap_or(false) {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            // Try to extract a display name from the YAML
            let name = match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if let Ok(manifest) = serde_norway::from_str::<YamlManifest>(&content) {
                        manifest.name.clone().unwrap_or(stem.clone())
                    } else {
                        stem.clone()
                    }
                }
                Err(_) => stem.clone(),
            };
            suites.push((stem, name));
        }
    }
    Ok(suites)
}

/// Return suite entries for the "ci_smoke" suite — first checks for YAML with
/// that name.
fn resolve_included_manifest(incl_path: &Path, sections: &[String]) -> Result<Vec<ResolvedEntry>> {
    let content = std::fs::read_to_string(incl_path).map_err(crate::error::SyscityError::Io)?;
    let manifest: YamlManifest = serde_norway::from_str(&content).map_err(|e| {
        crate::error::SyscityError::Validation(format!(
            "Cannot parse included {}: {}",
            incl_path.display(),
            e
        ))
    })?;
    let incl_dir = incl_path.parent().unwrap_or(Path::new("."));

    let mut entries = Vec::new();
    for (key, cat) in &manifest.categories {
        if !sections.is_empty() && !sections.contains(key) {
            continue;
        }
        for entry in &cat.suites {
            let task_path = resolve_path(incl_dir, &entry.path);
            entries.push(ResolvedEntry {
                task_path,
                task_filter: entry.task_filter.clone(),
                min_pass_rate: entry.min_pass_rate.unwrap_or(0.8),
                trials: entry.trials.unwrap_or(5),
                sampling_rate: entry.sampling_rate,
            });
        }
    }
    Ok(entries)
}

/// Result of loading a task YAML file — tasks + optional skill evaluation
/// design.
pub struct LoadedTaskFile {
    pub tasks: Vec<EvalTask>,
    pub skill_design: Option<SkillEvalDesign>,
}

/// Load tasks from a single YAML task file.
///
/// Each YAML file contains a `tasks:` list. Returns a `LoadedTaskFile` with
/// one entry per task and an optional skill evaluation design.
pub fn load_tasks(yaml_path: &Path) -> Result<LoadedTaskFile> {
    let content = std::fs::read_to_string(yaml_path).map_err(crate::error::SyscityError::Io)?;
    let file: YamlTaskFile = serde_norway::from_str(&content).map_err(|e| {
        crate::error::SyscityError::Validation(format!(
            "Cannot parse {}: {}",
            yaml_path.display(),
            e
        ))
    })?;

    let tasks: Result<Vec<EvalTask>> = file.tasks.into_iter().map(convert_task).collect();
    Ok(LoadedTaskFile {
        tasks: tasks?,
        skill_design: file.skill_eval_design,
    })
}

/// Load a named suite from a manifest file, resolving all referenced task
/// files and building a single [`EvalSuite`].
///
/// `manifest_path` — path to a `.yaml` file in `evals/suites/`.
/// `suite_name` — for registry-style manifests this selects a category
/// (e.g. `"capability"`, `"regression"`); for flat manifests
/// (ci_smoke.yaml) it is ignored — the entire file is loaded.
pub fn load_suite(manifest_path: &Path, suite_name: &str) -> Result<EvalSuite> {
    let content = std::fs::read_to_string(manifest_path).map_err(crate::error::SyscityError::Io)?;
    let manifest: YamlManifest = serde_norway::from_str(&content).map_err(|e| {
        crate::error::SyscityError::Validation(format!(
            "Cannot parse {}: {}",
            manifest_path.display(),
            e
        ))
    })?;

    let manifest_dir = manifest_path.parent().unwrap_or(Path::new("."));

    // Resolve tasks from the manifest
    let mut all_entries: Vec<ResolvedEntry> = Vec::new();
    let mut default_trials = manifest.trials.unwrap_or(5);
    let mut default_min_pass_rate = manifest.min_pass_rate.unwrap_or(0.8);

    // 1. Flat `tasks:` list (ci_smoke.yaml style)
    for task_ref in &manifest.tasks {
        let task_path = resolve_path(manifest_dir, &task_ref.path);
        let loaded = load_tasks(&task_path)?;
        let filtered: Vec<(String, String)> = loaded
            .tasks
            .into_iter()
            .filter_map(|t| {
                let id = t.id.clone();
                let input = t.input.clone();
                if let Some(ref filter) = task_ref.task_filter {
                    if id.contains(filter) {
                        Some((id, input))
                    } else {
                        None
                    }
                } else {
                    Some((id, input))
                }
            })
            .collect();
        for (_id, _input) in &filtered {
            all_entries.push(ResolvedEntry {
                task_path: task_path.clone(),
                task_filter: task_ref.task_filter.clone(),
                min_pass_rate: task_ref.min_pass_rate.unwrap_or(default_min_pass_rate),
                trials: task_ref.trials.unwrap_or(default_trials),
                sampling_rate: task_ref.sampling_rate,
            });
        }
    }

    // 2. Category-grouped suites (registry.yaml style)
    for (key, cat) in &manifest.categories {
        if !suite_name.is_empty() && key != suite_name {
            continue;
        }
        default_trials = manifest.trials.unwrap_or(5);
        default_min_pass_rate = manifest.min_pass_rate.unwrap_or(0.8);

        for entry in &cat.suites {
            let task_path = resolve_path(manifest_dir, &entry.path);
            all_entries.push(ResolvedEntry {
                task_path,
                task_filter: entry.task_filter.clone(),
                min_pass_rate: entry.min_pass_rate.unwrap_or(default_min_pass_rate),
                trials: entry.trials.unwrap_or(default_trials),
                sampling_rate: entry.sampling_rate,
            });
        }
    }

    // 3. Includes (release_gate.yaml style)
    for incl in &manifest.includes {
        let incl_path = resolve_path(manifest_dir, &incl.path);
        let mut entries = resolve_included_manifest(&incl_path, &incl.sections)?;
        all_entries.append(&mut entries);
    }

    // Deduplicate by task_path + task_filter
    all_entries.sort_by(|a, b| {
        a.task_path
            .cmp(&b.task_path)
            .then(a.task_filter.cmp(&b.task_filter))
    });
    all_entries.dedup_by(|a, b| a.task_path == b.task_path && a.task_filter == b.task_filter);

    // Load all tasks
    let mut tasks = Vec::new();
    let mut skill_designs = Vec::new();
    for entry in &all_entries {
        let loaded = load_tasks(&entry.task_path)?;
        if let Some(skill) = loaded.skill_design {
            skill_designs.push(skill);
        }
        if let Some(ref filter) = entry.task_filter {
            for t in loaded.tasks {
                if t.id.contains(filter) {
                    tasks.push(t);
                }
            }
        } else {
            tasks.extend(loaded.tasks);
        }
    }

    // Determine suite identity
    let suite_id = if suite_name.is_empty() {
        manifest
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| {
                manifest_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        suite_name.to_string()
    };

    let category = if let Some(cat) = manifest.categories.get(suite_name) {
        parse_category(cat.category.as_deref().unwrap_or("capability"))
    } else {
        SuiteCategory::Capability
    };

    let agent_type = manifest.agent_type.as_deref().map(parse_agent_type);

    Ok(EvalSuite {
        id: suite_id,
        name: manifest.name.unwrap_or_default(),
        category,
        tasks,
        min_pass_rate: default_min_pass_rate,
        trials: default_trials,
        continuous_success_required: manifest.continuous_success_required.unwrap_or(false),
        sampling_rate: manifest.sampling_rate.unwrap_or(1.0),
        tags: vec![],
        agent_type,
        skill_designs,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Internal helpers
// ═══════════════════════════════════════════════════════════════════════════

/// A resolved task-file reference after manifest processing.
#[derive(Debug, Clone, PartialEq)]
struct ResolvedEntry {
    task_path: PathBuf,
    task_filter: Option<String>,
    min_pass_rate: f64,
    trials: usize,
    sampling_rate: Option<f64>,
}

/// Resolve a relative YAML path against the manifest directory.
fn resolve_path(manifest_dir: &Path, rel: &str) -> PathBuf {
    let p = manifest_dir.join(rel);
    if p.exists() {
        p
    } else {
        // Fallback: resolve against project root / evals
        default_evals_dir().join(rel)
    }
}

/// Convert a single `YamlTask` into an `EvalTask`.
fn convert_task(yt: YamlTask) -> Result<EvalTask> {
    let conditions: Vec<GoalCondition> = yt
        .conditions
        .into_iter()
        .filter_map(convert_condition)
        .collect();

    let criteria = yt.criteria.map(|yc| {
        let dimensions: Vec<QualityDimension> =
            yc.dimensions.iter().map(|d| parse_dimension(d)).collect();
        QualityCriteria {
            dimensions,
            thresholds: yc.thresholds,
        }
    });

    let setup: Vec<SetupCommand> = yt
        .setup
        .into_iter()
        .map(|c| SetupCommand { command: c.command })
        .collect();
    let cleanup: Vec<SetupCommand> = yt
        .cleanup
        .into_iter()
        .map(|c| SetupCommand { command: c.command })
        .collect();

    let source = match yt.source.as_deref() {
        Some("extended") => EvalTaskSource::Extended,
        Some("online") => EvalTaskSource::Online,
        Some("badcase") => EvalTaskSource::BadcaseRecycle,
        _ => EvalTaskSource::ExpertDesign,
    };

    let turns: Vec<TurnInput> = yt
        .turns
        .into_iter()
        .map(|yti| TurnInput {
            user_message: yti.user_message,
            conditions: yti
                .conditions
                .into_iter()
                .filter_map(convert_condition)
                .collect(),
        })
        .collect();

    let session_conditions: Vec<GoalCondition> = yt
        .session_conditions
        .into_iter()
        .filter_map(convert_condition)
        .collect();

    Ok(EvalTask {
        id: yt.id,
        description: yt.description,
        input: yt.input,
        user_id: "eval_user".to_string(),
        conditions,
        criteria,
        expected_behavior: yt.expected_behavior,
        source,
        failure_reason: yt.failure_reason,
        setup,
        cleanup,
        agent_type: yt.agent_type.as_deref().map(parse_agent_type),
        turns,
        session_conditions,
        difficulty: yt.difficulty,
        coverage: yt.coverage,
        trials: None,
    })
}

/// Convert a single `YamlCondition` into a `GoalCondition`.
///
/// Returns `None` if the condition type is unknown (logged as warning).
fn convert_condition(yc: YamlCondition) -> Option<GoalCondition> {
    match yc.cond_type.as_str() {
        "exit_code" => Some(GoalCondition::ExitCode {
            command: yc.command,
            expected: yc.expected,
        }),
        "pattern" => {
            let must_contain = yc
                .must_contain
                .map(|v| yaml_value_to_string(&v))
                .unwrap_or_default();
            Some(GoalCondition::Pattern {
                command: yc.command,
                must_contain,
            })
        }
        "file_exists" => Some(GoalCondition::FileExists { path: yc.path }),
        "must_not_contain" => {
            let must_not_contain = yc
                .must_contain
                .map(|v| yaml_value_to_string(&v))
                .unwrap_or_default();
            Some(GoalCondition::MustNotContain {
                command: yc.command,
                must_not_contain,
            })
        }
        "numeric" => {
            let operator = parse_operator(&yc.operator);
            Some(GoalCondition::Numeric {
                command: yc.command,
                operator,
                threshold: yc.threshold.unwrap_or(0.0),
            })
        }
        other => {
            warn!("Unknown condition type '{}' — skipping", other);
            None
        }
    }
}

/// Parse `must_contain` / `expected` from YAML `Value` to `String`.
///
/// Handles both `must_contain: 1` (integer) and `must_contain: "text"`
/// (string).
fn yaml_value_to_string(v: &serde_norway::Value) -> String {
    match v {
        serde_norway::Value::String(s) => s.clone(),
        serde_norway::Value::Number(n) => n.to_string(),
        serde_norway::Value::Bool(b) => b.to_string(),
        other => format!("{:?}", other),
    }
}

/// Parse a dimension label into a `QualityDimension`.
pub fn parse_dimension(s: &str) -> QualityDimension {
    match s {
        "factual_accuracy" => QualityDimension::FactualAccuracy,
        "completeness" => QualityDimension::Completeness,
        "consistency" => QualityDimension::Consistency,
        "clarity" => QualityDimension::Clarity,
        "actionable" => QualityDimension::Actionable,
        "safety" => QualityDimension::Safety,
        "instruction_following" => QualityDimension::InstructionFollowing,
        "context_retention" => QualityDimension::ContextRetention,
        "goal_switch" => QualityDimension::GoalSwitch,
        "emotion_handling" => QualityDimension::EmotionHandling,
        "evidence_consistency" => QualityDimension::EvidenceConsistency,
        other => QualityDimension::Custom(other.to_string()),
    }
}

/// Parse the operator string for numeric conditions.
fn parse_operator(s: &str) -> crate::goal::condition::Comparison {
    match s {
        "gt" | ">" => crate::goal::condition::Comparison::Gt,
        "lt" | "<" => crate::goal::condition::Comparison::Lt,
        "gte" | ">=" | "ge" => crate::goal::condition::Comparison::Ge,
        "lte" | "<=" | "le" => crate::goal::condition::Comparison::Le,
        _ => crate::goal::condition::Comparison::Eq,
    }
}

/// Parse a category string into `SuiteCategory`.
fn parse_category(s: &str) -> SuiteCategory {
    match s {
        "capability" => SuiteCategory::Capability,
        "regression" => SuiteCategory::Regression,
        "adversarial" => SuiteCategory::Adversarial,
        "multi_turn" => SuiteCategory::MultiTurnHard,
        _ => SuiteCategory::Capability,
    }
}

/// Parse an agent type string into `AgentType`.
fn parse_agent_type(s: &str) -> AgentType {
    match s {
        "knowledge_qa" => AgentType::KnowledgeQA,
        "task_execution" => AgentType::TaskExecution,
        "reasoning_decision" => AgentType::ReasoningDecision,
        "multi_turn_guide" => AgentType::MultiTurnGuide,
        "creative_generation" => AgentType::CreativeGeneration,
        "multi_agent" => AgentType::MultiAgent,
        other => {
            warn!("Unknown agent_type '{}' — defaulting to KnowledgeQA", other);
            AgentType::KnowledgeQA
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_old_badcase_yaml_without_labels_parses_with_defaults() {
        // Pre-existing badcase YAML has no difficulty/coverage fields — the
        // loader must default them so the regression suite keeps loading.
        let yaml = r#"
            tasks:
            - id: legacy_bc
              input: hello
              source: badcase
              failure_reason: crit
        "#;
        let file: YamlTaskFile = serde_norway::from_str(yaml).unwrap();
        let tasks: Vec<EvalTask> = file
            .tasks
            .into_iter()
            .map(convert_task)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].difficulty, "medium");
        assert!(tasks[0].coverage.is_empty());
        assert_eq!(tasks[0].source, EvalTaskSource::BadcaseRecycle);
    }

    #[test]
    fn test_labeled_badcase_yaml_roundtrips_through_loader() {
        let yaml = r#"
            tasks:
            - id: labeled_bc
              input: hello
              source: badcase
              difficulty: hard
              coverage: [tools, routing]
        "#;
        let file: YamlTaskFile = serde_norway::from_str(yaml).unwrap();
        let tasks: Vec<EvalTask> = file
            .tasks
            .into_iter()
            .map(convert_task)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(tasks[0].difficulty, "hard");
        assert_eq!(tasks[0].coverage, vec!["tools", "routing"]);
    }

    #[test]
    fn test_convert_yaml_value_to_string() {
        let yaml: serde_norway::Value = serde_norway::from_str("42").unwrap();
        assert_eq!(yaml_value_to_string(&yaml), "42");

        let yaml: serde_norway::Value = serde_norway::from_str("\"hello\"").unwrap();
        assert_eq!(yaml_value_to_string(&yaml), "hello");

        let yaml: serde_norway::Value = serde_norway::from_str("true").unwrap();
        assert_eq!(yaml_value_to_string(&yaml), "true");
    }

    #[test]
    fn test_convert_condition_pattern() {
        let yc = YamlCondition {
            cond_type: "pattern".into(),
            command: "grep -c 'web_search' /tmp/log".into(),
            must_contain: Some(serde_norway::Value::Number(1.into())),
            expected: None,
            path: String::new(),
            operator: String::new(),
            threshold: None,
        };
        let gc = convert_condition(yc).unwrap();
        match gc {
            GoalCondition::Pattern { must_contain, .. } => {
                assert_eq!(must_contain, "1");
            }
            _ => panic!("Expected Pattern condition"),
        }
    }

    #[test]
    fn test_convert_condition_exit_code() {
        let yc = YamlCondition {
            cond_type: "exit_code".into(),
            command: "grep -c 'test' /tmp/response.txt".into(),
            must_contain: None,
            expected: Some(0),
            path: String::new(),
            operator: String::new(),
            threshold: None,
        };
        let gc = convert_condition(yc).unwrap();
        match gc {
            GoalCondition::ExitCode { expected, .. } => {
                assert_eq!(expected, Some(0));
            }
            _ => panic!("Expected ExitCode condition"),
        }
    }

    #[test]
    fn test_list_suites() {
        let evals_dir = project_root().join("evals");
        if evals_dir.exists() {
            let suites = list_suites(&evals_dir).unwrap();
            assert!(!suites.is_empty(), "Should find at least one suite");
            let names: Vec<&str> = suites.iter().map(|(s, _)| s.as_str()).collect();
            assert!(names.contains(&"ci_smoke"), "Should contain ci_smoke");
        }
    }

    #[test]
    fn test_load_web_search_tasks() {
        let path = project_root().join("evals/capability/web_search.yaml");
        if path.exists() {
            let loaded = load_tasks(&path).unwrap();
            let tasks = loaded.tasks;
            assert!(!tasks.is_empty(), "Should load tasks");
            let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
            assert!(ids.contains(&"web_search_current_event"));
        }
    }

    #[test]
    fn test_parse_dimension_known() {
        assert_eq!(parse_dimension("factual_accuracy"), QualityDimension::FactualAccuracy);
        assert_eq!(parse_dimension("safety"), QualityDimension::Safety);
        assert_eq!(
            parse_dimension("instruction_following"),
            QualityDimension::InstructionFollowing
        );
    }

    #[test]
    fn test_parse_dimension_custom() {
        let d = parse_dimension("custom_metric");
        assert_eq!(d, QualityDimension::Custom("custom_metric".to_string()));
    }
}
