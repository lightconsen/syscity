//! Layered scoring engine — Coarse → Fine → Human Review.
//!
//! Implements the three-tier screening architecture from §06-9:
//! 1. Coarse (Code Scorer) — fast deterministic checks via GoalCondition
//! 2. Fine (LLM Judge) — semantic evaluation via Critic
//! 3. Human Review — routing for low-confidence / conflict cases

use serde::{Deserialize, Serialize};

use crate::agent::reflection::critic::Critic;
use crate::agent::reflection::types::{Critique, QualityCriteria};
use crate::eval::human_review::{route_case, HumanReviewCase, HumanReviewStore, ReviewStatus};
use crate::eval::rca::ProblemPhenomenon;
use crate::goal::condition::{CheckResult, GoalCondition};

/// Pre-defined risk signal patterns for coarse-layer screening.
///
/// These are cheap, deterministic checks on the agent's response text that
/// catch common failure modes before fine-grained (LLM) evaluation.
#[derive(Debug, Clone)]
pub struct RiskSignalChecker {
    /// Patterns that are high-risk (contains check — case-insensitive).
    pub high_risk_patterns: Vec<String>,
    /// Minimum response length in characters (0 = disable).
    pub min_response_length: usize,
    /// Maximum tool calls allowed (0 = disable).
    pub max_tool_calls: usize,
}

impl Default for RiskSignalChecker {
    fn default() -> Self {
        Self {
            high_risk_patterns: vec![
                "password".into(),
                "api_key".into(),
                "secret".into(),
                "refund".into(),
                "i cannot".into(),
                "i am unable".into(),
                "as an ai".into(),
            ],
            min_response_length: 10,
            max_tool_calls: 50,
        }
    }
}

/// Lightweight snapshot of a completed turn, used for online risk scanning.
///
/// Deliberately distinct from [`crate::observe::record::TurnRecord`]: the
/// persisted record truncates its text previews, while online risk scanning
/// must see the full, untruncated input/response as they exist at hook time.
#[derive(Debug, Clone)]
pub struct RiskTurnInput {
    pub input: String,
    pub response: String,
    pub tool_call_count: usize,
}

impl RiskSignalChecker {
    /// Run all risk checks against a response. Returns a list of risk reasons
    /// (empty = no risks detected).
    pub fn check(&self, response: &str, tool_call_count: usize) -> Vec<String> {
        let mut risks = Vec::new();

        // High-risk pattern check
        let response_lower = response.to_lowercase();
        for pattern in &self.high_risk_patterns {
            if response_lower.contains(pattern) {
                risks.push(format!("high-risk pattern '{}' found in response", pattern));
            }
        }

        // Minimum response length
        if self.min_response_length > 0 && response.len() < self.min_response_length {
            risks.push(format!(
                "response too short ({} < {} chars)",
                response.len(),
                self.min_response_length
            ));
        }

        // Max tool calls
        if self.max_tool_calls > 0 && tool_call_count > self.max_tool_calls {
            risks.push(format!(
                "too many tool calls ({} > {})",
                tool_call_count, self.max_tool_calls
            ));
        }

        risks
    }

    /// Scan a completed turn (input + response) for risk signals.
    ///
    /// This is the online hook the harness uses to auto-collect badcases:
    /// a non-empty result means the turn should land in the pending pool.
    /// Delegates to [`check`](Self::check) and adds an input-side signal for
    /// degenerate empty prompts.
    pub fn scan_turn(&self, record: &RiskTurnInput) -> Vec<String> {
        let mut risks = self.check(&record.response, record.tool_call_count);
        if record.input.trim().is_empty() {
            risks.push("empty user input".to_string());
        }
        risks
    }
}

/// Which screening layer produced the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreeningLayer {
    /// Passed or failed by rule Scorer (cheap, deterministic).
    Coarse,
    /// Passed or failed by LLM Judge (semantic).
    Fine,
    /// Needs human review (conflict / low confidence).
    HumanReview,
}

/// Final verdict from the scoring pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    Fail,
    /// Not enough information to decide — route to human.
    InsufficientInfo,
}

/// Output of the layered scoring pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringOutput {
    pub verdict: Verdict,
    pub score: f64,
    /// Phenomenon classification (for downstream RCA).
    pub problem_category: Option<ProblemPhenomenon>,
    /// Confidence in the verdict (0.0–1.0).
    pub confidence: f64,
    /// Human-readable judgment basis.
    pub judgment_basis: String,
    /// Which layer produced this output.
    pub screening_layer: ScreeningLayer,
}

/// One trial to score: what was asked, what was answered, and where it sits
/// in its task's trial sequence.
pub struct ScoredTrial<'a> {
    /// Identifier of the eval task this trial belongs to.
    pub task_id: &'a str,
    /// The original task prompt shown to the agent.
    pub input: &'a str,
    /// The agent's final answer.
    pub response: &'a str,
    /// The full action/observation trajectory leading to `response`.
    pub trajectory: &'a str,
    /// Goal conditions the response is checked against.
    pub conditions: &'a [GoalCondition],
    /// Optional extra quality criteria for fine scoring.
    pub criteria: Option<&'a QualityCriteria>,
    /// Zero-based position of this trial within the task.
    pub trial_index: usize,
}

/// Configuration for the layered scorer.
#[derive(Debug, Clone)]
pub struct ScorerConfig {
    /// Confidence threshold below which fine scoring is skipped (early pass).
    pub coarse_pass_threshold: f64,
    /// Confidence threshold below which fine scoring is skipped (early fail).
    pub coarse_fail_threshold: f64,
    /// Fine scorer confidence below which human review is triggered.
    pub fine_min_confidence: f64,
}

impl Default for ScorerConfig {
    fn default() -> Self {
        Self {
            coarse_pass_threshold: 0.9,
            coarse_fail_threshold: 0.3,
            fine_min_confidence: 0.6,
        }
    }
}

/// Layered scoring engine.
///
/// Implements the three-tier screening from §06-9:
/// ```text
/// Coarse (GoalCondition) → Fine (Critic) → HumanReview (routing)
/// ```
pub struct LayeredScorer {
    critic: Option<Critic>,
    config: ScorerConfig,
    risk_checker: RiskSignalChecker,
    review_store: Option<HumanReviewStore>,
    /// Fixed sampling rate for routing ordinary cases to human review (§三).
    sampling_rate: Option<f64>,
}

impl LayeredScorer {
    pub fn new(critic: Option<Critic>, config: ScorerConfig) -> Self {
        Self {
            critic,
            config,
            risk_checker: RiskSignalChecker::default(),
            review_store: None,
            sampling_rate: None,
        }
    }

    /// Set a custom risk signal checker.
    pub fn with_risk_checker(mut self, risk_checker: RiskSignalChecker) -> Self {
        self.risk_checker = risk_checker;
        self
    }

    /// Enable human review persistence for low-confidence cases.
    pub fn with_review_store(mut self, store: HumanReviewStore) -> Self {
        self.review_store = Some(store);
        self
    }

    /// Enable fixed-rate sampling of ordinary (non low-confidence) cases into
    /// human review (§三). `None` or `Some(0.0)` disables sampling; `Some(1.0)`
    /// always routes ordinary cases; intermediate values route with probability
    /// `rate`.
    pub fn with_sampling_rate(mut self, rate: Option<f64>) -> Self {
        self.sampling_rate = rate;
        self
    }

    /// Run score() and automatically persist InsufficientInfo results.
    ///
    /// When the verdict is `InsufficientInfo` and a `HumanReviewStore` is
    /// configured, the case is written to disk automatically.
    pub async fn score_and_review(&self, trial: ScoredTrial<'_>) -> ScoringOutput {
        let output = self
            .score(trial.conditions, trial.criteria, trial.response, trial.trajectory)
            .await;

        if let Some(ref store) = self.review_store {
            let case = HumanReviewCase {
                task_id: trial.task_id.to_string(),
                trial_index: trial.trial_index,
                input: trial.input.to_string(),
                response: trial.response.to_string(),
                trajectory: Some(trial.trajectory.to_string()),
                scoring_output: output.clone(),
                status: ReviewStatus::Pending,
                created_at: std::time::SystemTime::now(),
                human_verdict: None,
                human_comment: None,
            };
            // Route low-confidence / conflict cases unconditionally, and
            // ordinary cases with probability `sampling_rate` (§三).
            let mut rng = rand::rng();
            if let Err(e) = route_case(
                store,
                &case,
                output.verdict == Verdict::InsufficientInfo,
                self.sampling_rate,
                &mut rng,
            ) {
                tracing::warn!("Failed to persist review case: {}", e);
            }
        }

        output
    }

    /// Route an already-scored trial to human review without re-running the
    /// scoring pipeline (§三 harness hook).
    ///
    /// Unlike [`Self::score_and_review`], this does not call [`Self::score`]
    /// (which would re-run GoalCondition checks and the LLM Judge). The harness
    /// computes the signals itself and passes them in:
    ///
    /// - `base_reason` — the harness's "uncertain / judge-flagged" signal
    ///   (conditions mixed, or the LLM Judge did not pass). `true` always
    ///   routes; `false` routes only when the configured `sampling_rate`
    ///   selects the ordinary case.
    ///
    /// Returns `Ok(true)` when a review case was persisted, `Ok(false)` when
    /// skipped or no review store is configured. Errors are surfaced to the
    /// caller (the harness logs and ignores them so a trial never fails on
    /// review routing).
    pub async fn route_scored(
        &self,
        task_id: &str,
        trial_index: usize,
        input: &str,
        response: &str,
        trajectory: &str,
        base_reason: bool,
    ) -> crate::Result<bool> {
        let Some(store) = &self.review_store else {
            return Ok(false);
        };
        let case = HumanReviewCase {
            task_id: task_id.to_string(),
            trial_index,
            input: input.to_string(),
            response: response.to_string(),
            trajectory: Some(trajectory.to_string()),
            scoring_output: ScoringOutput {
                verdict: if base_reason {
                    Verdict::InsufficientInfo
                } else {
                    Verdict::Pass
                },
                score: 0.0,
                problem_category: None,
                confidence: 0.0,
                judgment_basis: "Harness post-score human-review routing".to_string(),
                screening_layer: ScreeningLayer::Fine,
            },
            status: ReviewStatus::Pending,
            created_at: std::time::SystemTime::now(),
            human_verdict: None,
            human_comment: None,
        };
        let mut rng = rand::rng();
        route_case(store, &case, base_reason, self.sampling_rate, &mut rng)
    }

    /// Run the full layered scoring pipeline.
    pub async fn score(
        &self,
        conditions: &[GoalCondition],
        criteria: Option<&QualityCriteria>,
        response: &str,
        trajectory: &str,
    ) -> ScoringOutput {
        // ── Risk signal check (pre-coarse) ─────────────────────────
        if !response.is_empty() {
            let risks = self.risk_checker.check(response, 0);
            if !risks.is_empty() {
                return ScoringOutput {
                    verdict: Verdict::InsufficientInfo,
                    score: 0.0,
                    problem_category: None,
                    confidence: 1.0,
                    judgment_basis: format!("Risk signal detected: {}", risks.join("; ")),
                    screening_layer: ScreeningLayer::Coarse,
                };
            }
        }

        // ── Coarse layer: GoalCondition checks ─────────────────────
        let mut condition_results = Vec::new();
        for cond in conditions {
            let result = cond.check().await;
            condition_results.push(result);
        }

        let all_passed = condition_results.iter().all(|r| r.passed);
        let all_failed = condition_results.iter().all(|r| !r.passed);
        let pass_ratio = if condition_results.is_empty() {
            1.0
        } else {
            condition_results.iter().filter(|r| r.passed).count() as f64
                / condition_results.len() as f64
        };

        let detail = condition_results
            .iter()
            .map(|r| format!("{}: {}", r.condition, if r.passed { "PASS" } else { "FAIL" }))
            .collect::<Vec<_>>()
            .join("; ");

        // If all conditions pass with high confidence, early pass
        if all_passed && pass_ratio >= self.config.coarse_pass_threshold {
            return ScoringOutput {
                verdict: Verdict::Pass,
                score: pass_ratio,
                problem_category: None,
                confidence: pass_ratio,
                judgment_basis: format!("Coarse scorer: all conditions pass. {}", detail),
                screening_layer: ScreeningLayer::Coarse,
            };
        }

        // If all conditions fail with high confidence, early fail
        if all_failed && pass_ratio <= self.config.coarse_fail_threshold {
            return ScoringOutput {
                verdict: Verdict::Fail,
                score: pass_ratio,
                problem_category: detect_category_from_conditions(&condition_results),
                confidence: 1.0 - pass_ratio,
                judgment_basis: format!("Coarse scorer: all conditions fail. {}", detail),
                screening_layer: ScreeningLayer::Coarse,
            };
        }

        // ── Fine layer: LLM Judge ─────────────────────────────────
        if let (Some(ref critic), Some(criteria)) = (&self.critic, criteria) {
            match critic.evaluate_trajectory(trajectory, criteria, None).await {
                Ok(critique) => {
                    let confidence = critique.overall_score;
                    let passed = critique.passed;

                    // If confidence is high enough, return fine verdict
                    if confidence >= self.config.fine_min_confidence {
                        let category = detect_category_from_critique(&critique);
                        return ScoringOutput {
                            verdict: if passed { Verdict::Pass } else { Verdict::Fail },
                            score: confidence,
                            problem_category: category,
                            confidence,
                            judgment_basis: format!(
                                "Fine scorer: overall={:.2}, dims={:?}",
                                critique.overall_score, critique.dimension_scores
                            ),
                            screening_layer: ScreeningLayer::Fine,
                        };
                    }

                    // Low confidence → route to human review
                    ScoringOutput {
                        verdict: Verdict::InsufficientInfo,
                        score: confidence,
                        problem_category: detect_category_from_critique(&critique),
                        confidence,
                        judgment_basis: format!(
                            "Low confidence ({:.2}), needs human review. Critique: {:?}",
                            confidence, critique.weaknesses
                        ),
                        screening_layer: ScreeningLayer::HumanReview,
                    }
                }
                Err(e) => ScoringOutput {
                    verdict: Verdict::InsufficientInfo,
                    score: 0.0,
                    problem_category: None,
                    confidence: 0.0,
                    judgment_basis: format!("Fine scorer error: {}", e),
                    screening_layer: ScreeningLayer::HumanReview,
                },
            }
        } else {
            // No Critic configured — return coarse result directly
            ScoringOutput {
                verdict: if all_passed {
                    Verdict::Pass
                } else {
                    Verdict::Fail
                },
                score: pass_ratio,
                problem_category: detect_category_from_conditions(&condition_results),
                confidence: pass_ratio,
                judgment_basis: format!("Coarse scorer only (no critic). {}", detail),
                screening_layer: ScreeningLayer::Coarse,
            }
        }
    }
}

/// Detect problem phenomenon from condition check results.
fn detect_category_from_conditions(results: &[CheckResult]) -> Option<ProblemPhenomenon> {
    for r in results {
        if !r.passed {
            let actual_lower = r.actual.to_lowercase();
            if actual_lower.contains("tool") || actual_lower.contains("not called") {
                return Some(ProblemPhenomenon::ToolNotCalled);
            }
            if actual_lower.contains("promise") || actual_lower.contains("承诺") {
                return Some(ProblemPhenomenon::OverPromise);
            }
            if actual_lower.contains("hallucinat") || actual_lower.contains("幻觉") {
                return Some(ProblemPhenomenon::Hallucination);
            }
        }
    }
    None
}

/// Detect problem phenomenon from critique weaknesses.
fn detect_category_from_critique(critique: &Critique) -> Option<ProblemPhenomenon> {
    for w in &critique.weaknesses {
        let wl = w.to_lowercase();
        if wl.contains("promise") || wl.contains("承诺") || wl.contains("over") {
            return Some(ProblemPhenomenon::OverPromise);
        }
        if wl.contains("hallucinat") || wl.contains("幻觉") || wl.contains("编造") {
            return Some(ProblemPhenomenon::Hallucination);
        }
        if wl.contains("factual") || wl.contains("事实") || wl.contains("错误") {
            return Some(ProblemPhenomenon::FactualError);
        }
        if wl.contains("tool") || wl.contains("not call") || wl.contains("未调用") {
            return Some(ProblemPhenomenon::ToolNotCalled);
        }
        if wl.contains("refus") || wl.contains("拒绝") {
            return Some(ProblemPhenomenon::RefusalError);
        }
        if wl.contains("off-topic") || wl.contains("答非所问") {
            return Some(ProblemPhenomenon::NonResponsive);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scorer_config_default() {
        let cfg = ScorerConfig::default();
        assert_eq!(cfg.coarse_pass_threshold, 0.9);
        assert_eq!(cfg.fine_min_confidence, 0.6);
    }

    #[test]
    fn test_risk_checker_no_risks() {
        let checker = RiskSignalChecker::default();
        let risks = checker.check("This is a perfectly safe response about the weather.", 3);
        assert!(risks.is_empty());
    }

    #[test]
    fn test_risk_checker_high_risk_pattern() {
        let checker = RiskSignalChecker::default();
        let risks = checker.check("Your password is 12345", 1);
        assert!(!risks.is_empty());
        assert!(risks[0].contains("password"));
    }

    #[test]
    fn test_risk_checker_too_short() {
        let checker = RiskSignalChecker::default();
        let risks = checker.check("Hi", 0);
        assert!(!risks.is_empty());
        assert!(risks[0].contains("too short"));
    }

    #[test]
    fn test_risk_checker_too_many_calls() {
        let checker = RiskSignalChecker::default();
        let risks =
            checker.check("This is a sufficiently long response to pass the length check.", 99);
        assert!(!risks.is_empty());
        assert!(risks.iter().any(|r| r.contains("too many")));
    }

    #[test]
    fn test_scan_turn_clean() {
        let checker = RiskSignalChecker::default();
        let record = RiskTurnInput {
            input: "What is the weather?".to_string(),
            response: "The weather today is sunny with a high of 24 degrees.".to_string(),
            tool_call_count: 1,
        };
        assert!(checker.scan_turn(&record).is_empty());
    }

    #[test]
    fn test_scan_turn_flags_response_risk() {
        let checker = RiskSignalChecker::default();
        let record = RiskTurnInput {
            input: "Show me the account".to_string(),
            response: "Your password is 12345".to_string(),
            tool_call_count: 0,
        };
        let risks = checker.scan_turn(&record);
        assert!(risks.iter().any(|r| r.contains("password")));
    }

    #[test]
    fn test_scan_turn_flags_empty_input() {
        let checker = RiskSignalChecker::default();
        let record = RiskTurnInput {
            input: "   ".to_string(),
            response: "I'm here to help.".to_string(),
            tool_call_count: 0,
        };
        let risks = checker.scan_turn(&record);
        assert!(risks.iter().any(|r| r.contains("empty user input")));
    }
}
