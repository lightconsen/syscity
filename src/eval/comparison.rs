//! Version comparison — paired bootstrap for statistical regression detection.
//!
//! Determines whether a new agent version is significantly better, worse, or
//! no different from a baseline, using bootstrap confidence intervals.
//!
//! # Example
//!
//! ```rust
//! use syscity::eval::compare_versions;
//!
//! let old = vec![true, true, true, true, false]; // 80%
//! let new = vec![true, true, true, true, true]; // 100%
//! let result = compare_versions(&old, &new, 10_000, 0.95);
//! assert_eq!(result.old_pass_rate, 0.8);
//! assert_eq!(result.new_pass_rate, 1.0);
//! assert!(result.delta > 0.0);
//! ```

use std::time::SystemTime;

use rand::RngExt;
use serde::{Deserialize, Serialize};

/// Verdict from a version comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonVerdict {
    /// New version is significantly better (CI entirely > 0).
    Improved,
    /// New version is significantly worse (CI entirely < 0).
    Regressed,
    /// No statistically significant difference (CI contains 0).
    NoSignificantChange,
    /// Not enough trials to make a determination.
    InsufficientData,
}

/// Result of comparing two agent versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionComparison {
    /// Verdict.
    pub verdict: ComparisonVerdict,
    /// Old version pass rate.
    pub old_pass_rate: f64,
    /// New version pass rate.
    pub new_pass_rate: f64,
    /// Observed difference (new - old).
    pub delta: f64,
    /// Bootstrap 95% CI of the difference.
    pub confidence_interval: (f64, f64),
    /// Number of bootstrap iterations used.
    pub bootstrap_iterations: usize,
    /// When the comparison was computed.
    pub computed_at: SystemTime,
}

impl std::fmt::Display for VersionComparison {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:?}] old={:.1}% → new={:.1}%, Δ={:+.1}% CI=({:+.1}%, {:+.1}%)",
            self.verdict,
            self.old_pass_rate * 100.0,
            self.new_pass_rate * 100.0,
            self.delta * 100.0,
            self.confidence_interval.0 * 100.0,
            self.confidence_interval.1 * 100.0,
        )
    }
}

/// Compare two sets of per-trial pass/fail results using bootstrap.
///
/// # Parameters
///
/// * `old_results` — per-trial pass/fail for the baseline version.
/// * `new_results` — per-trial pass/fail for the new version.
/// * `iterations` — number of bootstrap resamples (e.g., 10_000).
/// * `confidence_level` — e.g., 0.95 for 95% CI.
///
/// The two slices need not have the same length — each is resampled
/// independently to its own size.
pub fn compare_versions(
    old_results: &[bool],
    new_results: &[bool],
    iterations: usize,
    confidence_level: f64,
) -> VersionComparison {
    let n_old = old_results.len();
    let n_new = new_results.len();

    if n_old < 2 || n_new < 2 {
        let old_rate = pass_rate(old_results);
        let new_rate = pass_rate(new_results);
        return VersionComparison {
            verdict: ComparisonVerdict::InsufficientData,
            old_pass_rate: old_rate,
            new_pass_rate: new_rate,
            delta: new_rate - old_rate,
            confidence_interval: (0.0, 0.0),
            bootstrap_iterations: iterations,
            computed_at: SystemTime::now(),
        };
    }

    let old_rate = pass_rate(old_results);
    let new_rate = pass_rate(new_results);
    let delta = new_rate - old_rate;

    // Bootstrap the difference in pass rates
    let mut diffs = Vec::with_capacity(iterations);
    let mut rng = rand::rng();

    for _ in 0..iterations {
        let boot_old = bootstrap_sample(old_results, &mut rng);
        let boot_new = bootstrap_sample(new_results, &mut rng);
        diffs.push(boot_new - boot_old);
    }

    diffs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Percentile CI
    let alpha = 1.0 - confidence_level;
    let lower_idx = ((alpha / 2.0) * iterations as f64).round() as usize;
    let upper_idx = ((1.0 - alpha / 2.0) * iterations as f64).round() as usize;
    let lower_idx = lower_idx.min(iterations - 1);
    let upper_idx = upper_idx.min(iterations - 1);

    let ci = (diffs[lower_idx], diffs[upper_idx]);

    let verdict = if ci.0 > 0.0 {
        ComparisonVerdict::Improved
    } else if ci.1 < 0.0 {
        ComparisonVerdict::Regressed
    } else {
        ComparisonVerdict::NoSignificantChange
    };

    VersionComparison {
        verdict,
        old_pass_rate: old_rate,
        new_pass_rate: new_rate,
        delta,
        confidence_interval: ci,
        bootstrap_iterations: iterations,
        computed_at: SystemTime::now(),
    }
}

/// Compute pass rate from a slice of bools.
fn pass_rate(results: &[bool]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let passes = results.iter().filter(|&&r| r).count();
    passes as f64 / results.len() as f64
}

/// Draw a bootstrap resample with replacement and return the pass rate.
fn bootstrap_sample(results: &[bool], rng: &mut impl RngExt) -> f64 {
    let n = results.len();
    let mut passes = 0usize;
    for _ in 0..n {
        if results[rng.random_range(0..n)] {
            passes += 1;
        }
    }
    passes as f64 / n as f64
}

/// Extract per-trial pass/fail from `EvalSummary` for comparison.
///
/// Returns `(old_passed, new_passed)` tuples suitable for [`compare_versions`].
pub fn extract_trial_results(
    old_summary: &[crate::eval::harness::EvalSummary],
    new_summary: &[crate::eval::harness::EvalSummary],
    task_id: &str,
) -> (Vec<bool>, Vec<bool>) {
    let old = old_summary
        .iter()
        .find(|s| s.task_id == task_id)
        .map(|s| s.per_trial.iter().map(|t| t.passed).collect())
        .unwrap_or_default();

    let new = new_summary
        .iter()
        .find(|s| s.task_id == task_id)
        .map(|s| s.per_trial.iter().map(|t| t.passed).collect())
        .unwrap_or_default();

    (old, new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{EvalSummary, TrialResult};

    #[test]
    fn test_compare_identical() {
        let results = vec![true, true, true, false, false];
        let comp = compare_versions(&results, &results, 5_000, 0.95);
        assert_eq!(comp.verdict, ComparisonVerdict::NoSignificantChange);
        assert!((comp.delta).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compare_improved() {
        let old = vec![true, false, false, false, false]; // 20%
        let new = vec![true, true, true, true, false]; // 80%
        let comp = compare_versions(&old, &new, 5_000, 0.95);
        // With only 5 trials each, the bootstrap CI might still contain 0.
        // We test that the delta is positive and the pass rates are correct.
        assert!(comp.delta > 0.0, "delta should be positive, got {}", comp.delta);
        assert!((comp.old_pass_rate - 0.2).abs() < 0.01);
        assert!((comp.new_pass_rate - 0.8).abs() < 0.01);
        // With strong signal (20% vs 80%) and 5 trials each, expect improvement
        assert!(
            comp.verdict == ComparisonVerdict::Improved
                || comp.verdict == ComparisonVerdict::NoSignificantChange,
            "expected Improved or NoSignificantChange, got {:?}",
            comp.verdict
        );
    }

    #[test]
    fn test_compare_regressed() {
        let old = vec![true, true, true, true, false]; // 80%
        let new = vec![false, false, false, false, false]; // 0%
        let comp = compare_versions(&old, &new, 5_000, 0.95);
        assert!(comp.delta < 0.0, "delta should be negative, got {}", comp.delta);
        assert!((comp.old_pass_rate - 0.8).abs() < 0.01);
        assert!((comp.new_pass_rate - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_insufficient_data() {
        let old = vec![true];
        let new = vec![false];
        let comp = compare_versions(&old, &new, 1_000, 0.95);
        assert_eq!(comp.verdict, ComparisonVerdict::InsufficientData);
    }

    #[test]
    fn test_empty_old() {
        let old: Vec<bool> = vec![];
        let new = vec![true, true, true];
        let comp = compare_versions(&old, &new, 1_000, 0.95);
        assert_eq!(comp.verdict, ComparisonVerdict::InsufficientData);
    }

    #[test]
    fn test_extract_trial_results() {
        let old_summary = vec![EvalSummary {
            task_id: "task1".into(),
            total_trials: 3,
            per_trial: vec![
                TrialResult { passed: true, ..dummy_trial() },
                TrialResult { passed: false, ..dummy_trial() },
                TrialResult { passed: true, ..dummy_trial() },
            ],
            ..dummy_summary()
        }];
        let new_summary = vec![EvalSummary {
            task_id: "task1".into(),
            total_trials: 3,
            per_trial: vec![
                TrialResult { passed: true, ..dummy_trial() },
                TrialResult { passed: true, ..dummy_trial() },
                TrialResult { passed: true, ..dummy_trial() },
            ],
            ..dummy_summary()
        }];

        let (old, new) = extract_trial_results(&old_summary, &new_summary, "task1");
        assert_eq!(old, vec![true, false, true]);
        assert_eq!(new, vec![true, true, true]);
    }

    #[test]
    fn test_display_format() {
        let comp = VersionComparison {
            verdict: ComparisonVerdict::Improved,
            old_pass_rate: 0.5,
            new_pass_rate: 0.9,
            delta: 0.4,
            confidence_interval: (0.1, 0.7),
            bootstrap_iterations: 10_000,
            computed_at: SystemTime::now(),
        };
        let s = comp.to_string();
        assert!(s.contains("Improved"));
        assert!(s.contains("50.0%"));
        assert!(s.contains("90.0%"));
    }

    fn dummy_trial() -> TrialResult {
        TrialResult {
            trial_index: 0,
            response: String::new(),
            tool_calls: vec![],
            token_usage: None,
            duration_ms: 0,
            condition_results: vec![],
            conditions_passed: false,
            critique: None,
            critique_passed: false,
            skill_results: None,
            skill_passed: true,
            turn_results: vec![],
            session_condition_results: vec![],
            session_conditions_passed: true,
            passed: false,
        }
    }

    fn dummy_summary() -> EvalSummary {
        EvalSummary {
            task_id: String::new(),
            total_trials: 0,
            pass_rate: 0.0,
            at_least_once_success: false,
            continuous_success: false,
            confidence_interval: (0.0, 0.0),
            avg_dimension_scores: std::collections::HashMap::new(),
            avg_duration_ms: 0.0,
            avg_token_usage: None,
            skill_pass_rate: 1.0,
            skill_trigger_pass_rate: 1.0,
            skill_execution_pass_rate: 1.0,
            skill_quality_pass_rate: 1.0,
            skill_resilience_pass_rate: 1.0,
            skill_sub_metrics: std::collections::HashMap::new(),
            per_trial: vec![],
            completed_at: SystemTime::now(),
        }
    }
}
