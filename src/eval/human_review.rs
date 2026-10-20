//! Human Review routing — persist low-confidence scoring cases for manual
//! review (§06).
//!
//! When `LayeredScorer` returns `Verdict::InsufficientInfo` (low confidence or
//! risk signals), the case is written to `evals/review/` as a JSON file.
//! A CLI subcommand (`syscity eval review`) lists pending cases.
//!
//! §三 also supports a fixed sampling rate: when
//! `cfg.eval.human_review.sampling_rate` is `Some(rate)`, ordinary (non
//! low-confidence/conflict) cases are additionally routed with probability
//! `rate`. [`should_route_to_review`] / [`route_case`] implement the decision;
//! call sites: `LayeredScorer::score_and_review` (`src/eval/scorer.rs`) and the
//! harness post-score hook `LayeredScorer::route_scored`
//! (`src/eval/harness.rs`).
//!
//! # File format
//!
//! Each review case is a single JSON file at
//! `evals/review/<task_id>_<trial>_<timestamp>.json`:
//!
//! ```json
//! {
//!   "task_id": "web_search",
//!   "trial_index": 2,
//!   "input": "What is ...?",
//!   "response": "...",
//!   "scoring_output": { ... },
//!   "status": "Pending",
//!   "created_at": "..."
//! }
//! ```

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::eval::scorer::ScoringOutput;
use crate::Result;

/// Review status of a human review case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    /// Awaiting human review.
    Pending,
    /// Reviewed by human.
    Reviewed,
    /// Skipped (no action needed).
    Skipped,
}

/// A single case routed for human review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanReviewCase {
    /// Original eval task id.
    pub task_id: String,
    /// Trial index within the task.
    pub trial_index: usize,
    /// User input that triggered the case.
    pub input: String,
    /// Agent response.
    pub response: String,
    /// Action/observation trajectory leading to `response` (for review
    /// context). Absent on older on-disk cases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory: Option<String>,
    /// The scoring output (verdict, confidence, etc.).
    pub scoring_output: ScoringOutput,
    /// Review status.
    pub status: ReviewStatus,
    /// When this case was created.
    pub created_at: SystemTime,
    /// Optional human verdict override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_verdict: Option<String>,
    /// Optional human comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_comment: Option<String>,
}

impl HumanReviewCase {
    /// Generate a filename for this case.
    fn filename(&self) -> String {
        let ts = short_timestamp();
        sanitize_id(&format!("{}_{}_{}", self.task_id, self.trial_index, ts))
    }
}

/// Store for persisting and loading human review cases.
pub struct HumanReviewStore {
    /// Directory where review cases are stored.
    dir: PathBuf,
}

impl HumanReviewStore {
    /// Create a new store writing to `evals/review/` under `evals_dir`.
    pub fn new(evals_dir: &Path) -> Self {
        Self { dir: evals_dir.join("review") }
    }

    /// Persist a review case to disk as JSON.
    pub fn write_case(&self, case: &HumanReviewCase) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;

        let file_path = self.dir.join(format!("{}.json", case.filename()));
        let json = serde_json::to_string_pretty(case)
            .map_err(|e| crate::error::SyscityError::Validation(e.to_string()))?;
        std::fs::write(&file_path, json)?;

        Ok(file_path)
    }

    /// List all review cases, optionally filtered by status.
    pub fn list_cases(&self, status_filter: Option<ReviewStatus>) -> Result<Vec<PathBuf>> {
        if !self.dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut cases = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Some(ref filter) = status_filter {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(case) = serde_json::from_str::<HumanReviewCase>(&content) {
                            if case.status == *filter {
                                cases.push(path);
                            }
                        }
                    }
                } else {
                    cases.push(path);
                }
            }
        }
        cases.sort();
        Ok(cases)
    }

    /// Load a single review case from a file path.
    pub fn load_case(path: &Path) -> Result<HumanReviewCase> {
        let content = std::fs::read_to_string(path)?;
        let case: HumanReviewCase = serde_json::from_str(&content)
            .map_err(|e| crate::error::SyscityError::Validation(e.to_string()))?;
        Ok(case)
    }

    /// Update a review case's status (in-place file rewrite).
    pub fn update_status(&self, path: &Path, status: ReviewStatus) -> Result<()> {
        let mut case = Self::load_case(path)?;
        case.status = status;
        let json = serde_json::to_string_pretty(&case)
            .map_err(|e| crate::error::SyscityError::Validation(e.to_string()))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Count cases by status.
    pub fn count_by_status(&self) -> Result<(usize, usize, usize)> {
        let all = self.list_cases(None)?;
        let mut pending = 0usize;
        let mut reviewed = 0usize;
        let mut skipped = 0usize;

        for path in &all {
            if let Ok(case) = Self::load_case(path) {
                match case.status {
                    ReviewStatus::Pending => pending += 1,
                    ReviewStatus::Reviewed => reviewed += 1,
                    ReviewStatus::Skipped => skipped += 1,
                }
            }
        }

        Ok((pending, reviewed, skipped))
    }
}

// ── Fixed sampling-rate routing (§三) ──────────────────────────────────

/// Decide whether a single ordinary (non low-confidence/conflict) case is
/// sampled into human review by the configured fixed rate.
///
/// * `None` — no fixed sampling; ordinary cases are never routed by rate.
/// * `Some(r)` with `r <= 0.0` — never sample ordinary cases.
/// * `Some(r)` with `r >= 1.0` — always sample ordinary cases.
/// * `Some(r)` with `0.0 < r < 1.0` — sample with probability `r`.
///
/// Low-confidence/conflict cases are orthogonal and are always routed by
/// [`should_route_to_review`] regardless of this decision.
pub fn sampling_rate_hit(rate: Option<f64>, rng: &mut impl rand::RngExt) -> bool {
    match rate {
        None => false,
        Some(r) if r <= 0.0 => false,
        Some(r) if r >= 1.0 => true,
        Some(r) => rng.random::<f64>() < r,
    }
}

/// Decide whether a scored case should be routed to human review.
///
/// Returns `true` when `base_reason` holds (low-confidence / risk / conflict
/// routing today) or, failing that, when the configured fixed sampling rate
/// [`sampling_rate_hit`] selects the case.
pub fn should_route_to_review(
    base_reason: bool,
    sampling_rate: Option<f64>,
    rng: &mut impl rand::RngExt,
) -> bool {
    base_reason || sampling_rate_hit(sampling_rate, rng)
}

/// Route a case to human review, persisting it when the decision is `true`.
///
/// `base_reason` marks the case as low-confidence / conflict: it always routes.
/// When it is `false`, the configured `sampling_rate` decides whether the
/// ordinary case is sampled. Returns `Ok(true)` when the case was written to
/// disk, `Ok(false)` when it was skipped.
pub fn route_case(
    store: &HumanReviewStore,
    case: &HumanReviewCase,
    base_reason: bool,
    sampling_rate: Option<f64>,
    rng: &mut impl rand::RngExt,
) -> Result<bool> {
    if !should_route_to_review(base_reason, sampling_rate, rng) {
        return Ok(false);
    }
    store.write_case(case)?;
    Ok(true)
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn short_timestamp() -> String {
    let dur = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}", dur.as_secs())
}

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
    use crate::eval::scorer::{ScreeningLayer, Verdict};

    fn dummy_scoring_output() -> ScoringOutput {
        ScoringOutput {
            verdict: Verdict::InsufficientInfo,
            score: 0.4,
            problem_category: None,
            confidence: 0.3,
            judgment_basis: "Low confidence, needs review".into(),
            screening_layer: ScreeningLayer::Fine,
        }
    }

    /// An ordinary (non low-confidence/conflict) scoring output.
    fn dummy_scoring_output_pass() -> ScoringOutput {
        ScoringOutput {
            verdict: Verdict::Pass,
            score: 0.9,
            problem_category: None,
            confidence: 0.9,
            judgment_basis: "Confident pass".into(),
            screening_layer: ScreeningLayer::Fine,
        }
    }

    fn dummy_case(task_id: &str, output: ScoringOutput) -> HumanReviewCase {
        HumanReviewCase {
            task_id: task_id.into(),
            trial_index: 0,
            input: "input".into(),
            response: "response".into(),
            trajectory: None,
            scoring_output: output,
            status: ReviewStatus::Pending,
            created_at: SystemTime::now(),
            human_verdict: None,
            human_comment: None,
        }
    }

    #[test]
    fn test_human_review_case_serde_roundtrip() {
        let case = HumanReviewCase {
            task_id: "test_task".into(),
            trial_index: 1,
            input: "Hello".into(),
            response: "Hi there".into(),
            trajectory: None,
            scoring_output: dummy_scoring_output(),
            status: ReviewStatus::Pending,
            created_at: SystemTime::now(),
            human_verdict: None,
            human_comment: None,
        };

        let json = serde_json::to_string_pretty(&case).unwrap();
        let deserialized: HumanReviewCase = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.task_id, "test_task");
        assert_eq!(deserialized.trial_index, 1);
        assert_eq!(deserialized.status, ReviewStatus::Pending);
    }

    #[test]
    fn test_store_write_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanReviewStore::new(dir.path());

        let case = HumanReviewCase {
            task_id: "web_search".into(),
            trial_index: 0,
            input: "test input".into(),
            response: "test response".into(),
            trajectory: None,
            scoring_output: dummy_scoring_output(),
            status: ReviewStatus::Pending,
            created_at: SystemTime::now(),
            human_verdict: None,
            human_comment: None,
        };

        let path = store.write_case(&case).unwrap();
        assert!(path.exists());

        let cases = store.list_cases(None).unwrap();
        assert_eq!(cases.len(), 1);

        let loaded = HumanReviewStore::load_case(&cases[0]).unwrap();
        assert_eq!(loaded.task_id, "web_search");
    }

    #[test]
    fn test_store_count_by_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanReviewStore::new(dir.path());

        let case1 = HumanReviewCase {
            task_id: "t1".into(),
            trial_index: 0,
            input: "i1".into(),
            response: "r1".into(),
            trajectory: None,
            scoring_output: dummy_scoring_output(),
            status: ReviewStatus::Pending,
            created_at: SystemTime::now(),
            human_verdict: None,
            human_comment: None,
        };
        let case2 = HumanReviewCase {
            task_id: "t2".into(),
            trial_index: 0,
            input: "i2".into(),
            response: "r2".into(),
            trajectory: None,
            scoring_output: dummy_scoring_output(),
            status: ReviewStatus::Reviewed,
            created_at: SystemTime::now(),
            human_verdict: Some("Pass".into()),
            human_comment: Some("Looks good".into()),
        };

        store.write_case(&case1).unwrap();
        store.write_case(&case2).unwrap();

        let (pending, reviewed, skipped) = store.count_by_status().unwrap();
        assert_eq!(pending, 1);
        assert_eq!(reviewed, 1);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn test_update_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanReviewStore::new(dir.path());

        let case = HumanReviewCase {
            task_id: "t1".into(),
            trial_index: 0,
            input: "i1".into(),
            response: "r1".into(),
            trajectory: None,
            scoring_output: dummy_scoring_output(),
            status: ReviewStatus::Pending,
            created_at: SystemTime::now(),
            human_verdict: None,
            human_comment: None,
        };

        let path = store.write_case(&case).unwrap();
        store.update_status(&path, ReviewStatus::Reviewed).unwrap();

        let loaded = HumanReviewStore::load_case(&path).unwrap();
        assert_eq!(loaded.status, ReviewStatus::Reviewed);
    }

    // ── Fixed sampling-rate routing (§三) ───────────────────────────────

    #[test]
    fn should_route_no_sampling_none_rate() {
        let mut rng = rand::rng();
        // Ordinary case (base=false) is never routed when sampling is disabled.
        for _ in 0..100 {
            assert!(!should_route_to_review(false, None, &mut rng));
        }
        // base=true always routes regardless of sampling.
        for _ in 0..100 {
            assert!(should_route_to_review(true, None, &mut rng));
        }
    }

    #[test]
    fn should_route_sampling_zero_and_one() {
        let mut rng = rand::rng();
        // Some(0.0) never routes ordinary cases.
        for _ in 0..50 {
            assert!(!should_route_to_review(false, Some(0.0), &mut rng));
        }
        // Some(1.0) always routes ordinary cases.
        for _ in 0..50 {
            assert!(should_route_to_review(false, Some(1.0), &mut rng));
        }
        // base=true always wins even when sampling is 0.0.
        for _ in 0..20 {
            assert!(should_route_to_review(true, Some(0.0), &mut rng));
        }
    }

    #[test]
    fn should_route_sampling_roughly_half() {
        let mut rng = rand::rng();
        let mut hits = 0usize;
        for _ in 0..100 {
            if should_route_to_review(false, Some(0.5), &mut rng) {
                hits += 1;
            }
        }
        assert!((20..=80).contains(&hits), "hits={hits}");
    }

    #[test]
    fn sampling_rate_hit_deterministic_boundaries() {
        // A stub RNG that always yields the same u64 pins the exact boundary of
        // `rng.random::<f64>() < r`.
        let mut lo = ConstantRng(0); // random::<f64>() == 0.0
        let mut hi = ConstantRng(u64::MAX); // random::<f64>() ≈ 1.0

        assert!(sampling_rate_hit(Some(0.5), &mut lo));
        assert!(!sampling_rate_hit(Some(0.5), &mut hi));
        // Fast paths never consult the RNG.
        assert!(!sampling_rate_hit(None, &mut lo));
        assert!(!sampling_rate_hit(Some(0.0), &mut hi));
        assert!(sampling_rate_hit(Some(1.0), &mut hi));
    }

    #[test]
    fn route_case_writes_when_routed() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanReviewStore::new(dir.path());
        let ordinary = dummy_case("sampled", dummy_scoring_output_pass());

        let mut rng = rand::rng();
        let routed = route_case(&store, &ordinary, false, Some(1.0), &mut rng).unwrap();
        assert!(routed);
        assert_eq!(store.list_cases(None).unwrap().len(), 1);
    }

    #[test]
    fn route_case_skips_when_not_routed() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanReviewStore::new(dir.path());
        let ordinary = dummy_case("skipped", dummy_scoring_output_pass());

        let mut rng = rand::rng();
        let routed = route_case(&store, &ordinary, false, None, &mut rng).unwrap();
        assert!(!routed);
        assert_eq!(store.list_cases(None).unwrap().len(), 0);
    }

    #[test]
    fn route_case_routes_low_confidence_without_sampling() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanReviewStore::new(dir.path());
        let low_conf = dummy_case("lowconf", dummy_scoring_output());

        let mut rng = rand::rng();
        let routed = route_case(&store, &low_conf, true, None, &mut rng).unwrap();
        assert!(routed);
        assert_eq!(store.list_cases(None).unwrap().len(), 1);
    }

    /// Deterministic RNG stub for tests: always returns a fixed value.
    struct ConstantRng(u64);

    impl rand::TryRng for ConstantRng {
        type Error = std::convert::Infallible;

        fn try_next_u32(&mut self) -> std::result::Result<u32, Self::Error> {
            Ok(self.0 as u32)
        }

        fn try_next_u64(&mut self) -> std::result::Result<u64, Self::Error> {
            Ok(self.0)
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), Self::Error> {
            for b in dest {
                *b = self.0 as u8;
            }
            Ok(())
        }
    }
}
