//! Judge calibration set — evaluate Critic accuracy against known-answer
//! cases (§06).
//!
//! Calibration cases are curated examples with known expected verdicts and
//! dimension scores. Running [`calibrate`] compares the Critic's output against
//! expectations and produces an accuracy report.
//!
//! # File format
//!
//! Calibration cases live in `evals/calibration/*.yaml`:
//!
//! ```yaml
//! cases:
//!   - id: "factual_001"
//!     input: "What is the capital of France?"
//!     trajectory: "user: ...\nassistant: Paris is the capital of France."
//!     criteria:
//!       dimensions: [factual_accuracy, completeness]
//!       thresholds:
//!         Factual Accuracy: 0.8
//!     expected:
//!       verdict: "Pass"
//!       dimension_scores:
//!         Factual Accuracy: 0.9
//!         Completeness: 0.8
//!     acceptable_deviation: 0.2
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::agent::reflection::critic::Critic;
use crate::agent::reflection::types::QualityCriteria;
use crate::Result;

// ── Types ───────────────────────────────────────────────────────────────

/// A single calibration case with known expected outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationCase {
    /// Unique case identifier.
    pub id: String,
    /// Input text (for reference / display).
    pub input: String,
    /// Full trajectory text to evaluate.
    pub trajectory: String,
    /// Quality criteria for the Critic.
    pub criteria: QualityCriteria,
    /// Expected verdict.
    pub expected: ExpectedCalibration,
    /// Max allowed deviation from expected dimension scores (0.0–1.0).
    #[serde(default = "default_deviation")]
    pub acceptable_deviation: f64,
}

fn default_deviation() -> f64 {
    0.2
}

/// Expected outcome for a calibration case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedCalibration {
    /// Expected verdict.
    pub verdict: String,
    /// Expected dimension scores (label → score).
    #[serde(default)]
    pub dimension_scores: HashMap<String, f64>,
}

/// Result of evaluating a single calibration case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationResult {
    pub case_id: String,
    pub expected_verdict: String,
    pub actual_verdict: String,
    pub verdict_match: bool,
    pub dimension_accuracy: f64,
    pub overall_score_deviation: f64,
    pub details: String,
}

/// Aggregate calibration report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationReport {
    pub total_cases: usize,
    pub matching_verdicts: usize,
    /// Verdict match rate (0.0–1.0).
    pub verdict_accuracy: f64,
    /// Average dimension score accuracy across all cases.
    pub avg_dimension_accuracy: f64,
    /// Average overall score deviation.
    pub avg_score_deviation: f64,
    /// Per-case breakdown.
    pub per_case: Vec<CalibrationResult>,
    /// When this calibration was run.
    pub timestamp: SystemTime,
}

// ── YAML structure ──────────────────────────────────────────────────────

/// Top-level YAML structure for calibration files.
#[derive(Debug, Deserialize)]
struct CalibrationYaml {
    cases: Vec<CalibrationCaseYaml>,
}

/// Per-case YAML structure.
#[derive(Debug, Deserialize)]
struct CalibrationCaseYaml {
    id: String,
    input: String,
    trajectory: String,
    #[serde(default)]
    criteria: Option<CalibrationCriteriaYaml>,
    expected: ExpectedCalibration,
    #[serde(default = "default_deviation")]
    acceptable_deviation: f64,
}

#[derive(Debug, Deserialize)]
struct CalibrationCriteriaYaml {
    #[serde(default)]
    dimensions: Vec<String>,
    #[serde(default)]
    thresholds: HashMap<String, f64>,
}

// ── Public API ──────────────────────────────────────────────────────────

/// Load calibration cases from a YAML file.
pub fn load_calibration_cases(path: &Path) -> Result<Vec<CalibrationCase>> {
    let content = std::fs::read_to_string(path)?;
    let yaml: CalibrationYaml = serde_norway::from_str(&content).map_err(|e| {
        crate::error::SyscityError::Validation(format!(
            "Cannot parse calibration file {:?}: {}",
            path, e
        ))
    })?;

    let cases: Result<Vec<CalibrationCase>> = yaml
        .cases
        .into_iter()
        .map(|yc| {
            let criteria = yc
                .criteria
                .map(|c| {
                    use crate::eval::loader;
                    let dimensions = c
                        .dimensions
                        .iter()
                        .map(|d| loader::parse_dimension(d))
                        .collect();
                    QualityCriteria {
                        dimensions,
                        thresholds: c.thresholds,
                    }
                })
                .unwrap_or_default();

            Ok(CalibrationCase {
                id: yc.id,
                input: yc.input,
                trajectory: yc.trajectory,
                criteria,
                expected: yc.expected,
                acceptable_deviation: yc.acceptable_deviation,
            })
        })
        .collect();

    cases
}

/// Run calibration: evaluate all cases against the Critic and produce a report.
pub async fn calibrate(critic: Arc<Critic>, cases: &[CalibrationCase]) -> CalibrationReport {
    let mut per_case = Vec::with_capacity(cases.len());
    let mut matching_verdicts = 0usize;
    let mut total_dim_accuracy = 0.0f64;
    let mut total_score_deviation = 0.0f64;

    for case in cases {
        let result = evaluate_case(&critic, case).await;
        if result.verdict_match {
            matching_verdicts += 1;
        }
        total_dim_accuracy += result.dimension_accuracy;
        total_score_deviation += result.overall_score_deviation;
        per_case.push(result);
    }

    let n = cases.len() as f64;
    CalibrationReport {
        total_cases: cases.len(),
        matching_verdicts,
        verdict_accuracy: if n > 0.0 {
            matching_verdicts as f64 / n
        } else {
            1.0
        },
        avg_dimension_accuracy: if n > 0.0 { total_dim_accuracy / n } else { 1.0 },
        avg_score_deviation: if n > 0.0 {
            total_score_deviation / n
        } else {
            0.0
        },
        per_case,
        timestamp: SystemTime::now(),
    }
}

async fn evaluate_case(critic: &Critic, case: &CalibrationCase) -> CalibrationResult {
    let critique = match critic
        .evaluate_trajectory(&case.trajectory, &case.criteria, None)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            return CalibrationResult {
                case_id: case.id.clone(),
                expected_verdict: case.expected.verdict.clone(),
                actual_verdict: "error".into(),
                verdict_match: false,
                dimension_accuracy: 0.0,
                overall_score_deviation: 1.0,
                details: format!("Critic error: {}", e),
            };
        }
    };

    let actual_verdict = if critique.passed { "Pass" } else { "Fail" };
    let verdict_match = actual_verdict == case.expected.verdict;

    // Compute dimension accuracy: average of (1 - |expected - actual|) per
    // dimension
    let mut dim_accuracy_sum = 0.0f64;
    let mut dim_count = 0usize;
    for (dim, expected_score) in &case.expected.dimension_scores {
        if let Some(actual_score) = critique.dimension_scores.get(dim) {
            let deviation = (expected_score - actual_score).abs();
            dim_accuracy_sum += (1.0 - deviation).max(0.0);
            dim_count += 1;
        }
    }
    let dimension_accuracy = if dim_count > 0 {
        dim_accuracy_sum / dim_count as f64
    } else {
        1.0
    };

    let overall_score_deviation = case
        .expected
        .dimension_scores
        .values()
        .copied()
        .next()
        .map(|exp| (exp - critique.overall_score).abs())
        .unwrap_or(0.0);

    CalibrationResult {
        case_id: case.id.clone(),
        expected_verdict: case.expected.verdict.clone(),
        actual_verdict: actual_verdict.into(),
        verdict_match,
        dimension_accuracy,
        overall_score_deviation,
        details: format!(
            "overall={:.2}, dims={:?}, weaknesses={:?}",
            critique.overall_score, critique.dimension_scores, critique.weaknesses
        ),
    }
}

/// Persist a calibration report to the history file.
pub fn save_calibration_report(evals_dir: &Path, report: &CalibrationReport) -> Result<PathBuf> {
    let cal_dir = evals_dir.join("calibration");
    std::fs::create_dir_all(&cal_dir)?;

    let history_path = cal_dir.join("history.jsonl");
    let json = serde_json::to_string(report)
        .map_err(|e| crate::error::SyscityError::Validation(e.to_string()))?;

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)?;
    writeln!(file, "{}", json)?;

    Ok(history_path)
}

/// Load calibration history (all previous reports).
pub fn load_calibration_history(evals_dir: &Path) -> Result<Vec<CalibrationReport>> {
    let history_path = evals_dir.join("calibration").join("history.jsonl");
    if !history_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&history_path)?;
    let mut reports = Vec::new();
    for line in content.lines() {
        if !line.trim().is_empty() {
            if let Ok(report) = serde_json::from_str::<CalibrationReport>(line) {
                reports.push(report);
            }
        }
    }
    Ok(reports)
}

/// Compute drift: compare latest report to the one before it.
pub fn detect_drift(reports: &[CalibrationReport]) -> Option<String> {
    if reports.len() < 2 {
        return None;
    }

    let latest = &reports[reports.len() - 1];
    let prev = &reports[reports.len() - 2];

    let verdict_delta = latest.verdict_accuracy - prev.verdict_accuracy;
    let dim_delta = latest.avg_dimension_accuracy - prev.avg_dimension_accuracy;

    if verdict_delta < -0.05 {
        Some(format!(
            "Verdict accuracy dropped {:.1}% (was {:.1}%, now {:.1}%)",
            verdict_delta.abs() * 100.0,
            prev.verdict_accuracy * 100.0,
            latest.verdict_accuracy * 100.0,
        ))
    } else if dim_delta < -0.05 {
        Some(format!(
            "Dimension accuracy dropped {:.1}% (was {:.1}%, now {:.1}%)",
            dim_delta.abs() * 100.0,
            prev.avg_dimension_accuracy * 100.0,
            latest.avg_dimension_accuracy * 100.0,
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_drift_not_enough_data() {
        let reports = vec![CalibrationReport {
            total_cases: 1,
            matching_verdicts: 1,
            verdict_accuracy: 1.0,
            avg_dimension_accuracy: 1.0,
            avg_score_deviation: 0.0,
            per_case: vec![],
            timestamp: SystemTime::now(),
        }];
        assert!(detect_drift(&reports).is_none());
    }

    #[test]
    fn test_detect_drift_no_drift() {
        let r1 = CalibrationReport {
            total_cases: 1,
            matching_verdicts: 1,
            verdict_accuracy: 0.9,
            avg_dimension_accuracy: 0.85,
            avg_score_deviation: 0.1,
            per_case: vec![],
            timestamp: SystemTime::now(),
        };
        let r2 = CalibrationReport {
            total_cases: 1,
            matching_verdicts: 1,
            verdict_accuracy: 0.9,
            avg_dimension_accuracy: 0.85,
            avg_score_deviation: 0.1,
            per_case: vec![],
            timestamp: SystemTime::now(),
        };
        assert!(detect_drift(&[r1, r2]).is_none());
    }

    #[test]
    fn test_detect_drift_verdict_drop() {
        let r1 = CalibrationReport {
            total_cases: 5,
            matching_verdicts: 5,
            verdict_accuracy: 1.0,
            avg_dimension_accuracy: 0.9,
            avg_score_deviation: 0.1,
            per_case: vec![],
            timestamp: SystemTime::now(),
        };
        let r2 = CalibrationReport {
            total_cases: 5,
            matching_verdicts: 4,
            verdict_accuracy: 0.8,
            avg_dimension_accuracy: 0.9,
            avg_score_deviation: 0.1,
            per_case: vec![],
            timestamp: SystemTime::now(),
        };
        let drift = detect_drift(&[r1, r2]);
        assert!(drift.is_some());
        assert!(drift.unwrap().contains("dropped"));
    }

    #[test]
    fn test_save_and_load_history() {
        let dir = tempfile::tempdir().unwrap();
        let report = CalibrationReport {
            total_cases: 3,
            matching_verdicts: 2,
            verdict_accuracy: 0.6667,
            avg_dimension_accuracy: 0.8,
            avg_score_deviation: 0.15,
            per_case: vec![],
            timestamp: SystemTime::now(),
        };

        save_calibration_report(dir.path(), &report).unwrap();
        let loaded = load_calibration_history(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!((loaded[0].verdict_accuracy - 0.6667).abs() < 0.01);
    }

    #[test]
    fn test_load_calibration_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_calibration_history(dir.path()).unwrap();
        assert!(loaded.is_empty());
    }
}
