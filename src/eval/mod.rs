//! Syscity Evaluation Framework
//!
//! Implements the three-tier evaluation methodology from the Agent 评测
//! article:
//!
//! - **Eval Harness** — N-trial execution engine with GoalCondition (Code
//!   Scorer) and Critic (LLM Judge) scoring, plus Wilson CI statistics.
//! - **RCA Pipeline** — Root cause analysis with ProblemPhenomenon ×
//!   CandidateModule mapping table, 5-step investigation flow.
//! - **Quality Gates** — Gateway lifecycle integration for pre-release gating.
//!
//! # Quick Start
//!
//! ```rust
//! use std::sync::Arc;
//! use syscity::agent::reflection::Critic;
//! use syscity::agent::Agent;
//! use syscity::eval::{EvalHarness, EvalTask};
//!
//! # async fn example(agent: Arc<Agent>, critic: Critic) -> Result<(), Box<dyn std::error::Error>> {
//! let task = EvalTask {
//!     id: "example_task".into(),
//!     input: "Hello".into(),
//!     ..Default::default()
//! };
//! let harness = EvalHarness::new(agent, Some(critic));
//! let summary = harness.run(task, 5).await?;
//! println!("Pass rate: {:.1}%", summary.pass_rate * 100.0);
//! # Ok(())
//! # }
//! ```
// INVARIANTS-NONE: eval artifacts are user-visible files that are parsed and validated at load time.

pub(crate) mod action;
pub(crate) mod agent_type;
pub(crate) mod comparison;
pub(crate) mod dataset;
pub(crate) mod harness;
pub(crate) mod loader;
pub(crate) mod rca;
pub(crate) mod recycle;
pub(crate) mod scorer;
pub(crate) mod skill_scorer;
pub(crate) mod standalone;

pub(crate) mod apply_patch;
pub(crate) mod calibration;
pub(crate) mod compression_gate;
pub(crate) mod decision_trace;
pub(crate) mod feedback_ops;
pub(crate) mod guardrail;
pub(crate) mod human_review;
pub(crate) mod multi_judge;
pub(crate) mod optimizer;
pub(crate) mod pending_badcase;
pub(crate) mod proposer;
pub(crate) mod sample_store;
pub(crate) mod verdict;

pub use action::{
    generate_action_items, load_action_items, write_action_items, ActionItem, ActionLevel,
    ImpactScope, Priority,
};
pub use agent_type::AgentType;
pub use apply_patch::{
    applied_evidence, apply_optimizer_patch, conflict_evidence, OptimizerPatch, PatchOutcome,
};
pub use calibration::{
    calibrate, detect_drift, load_calibration_cases, load_calibration_history,
    save_calibration_report, CalibrationCase, CalibrationReport, CalibrationResult,
};
pub use comparison::{
    compare_versions, extract_trial_results, ComparisonVerdict, VersionComparison,
};
pub use dataset::TurnInput;
pub use dataset::{
    DegradeExpectation, EvalSuite, EvalTask, EvalTaskSource, ExecutionCase, FailureMode,
    ParamMatcher, QualityCase, ResilienceCase, SkillEvalDesign, SuiteCategory, TriggerCase,
};
pub use decision_trace::{
    DecisionTrace, DecisionTraceStore, RecordTraceParams, TraceKind, TraceStatus,
};
pub use guardrail::{
    BreakerSnapshot, CircuitBreaker, NoopShadowEvaluator, OnlineSignalShadowEvaluator,
    ShadowEvaluator,
};
pub use harness::{
    EarlyStopConfig, EvalHarness, EvalSummary, ToolCallSummary, TrialResult, TurnResult,
};
pub use human_review::{HumanReviewCase, HumanReviewStore, ReviewStatus};
pub use loader::{
    default_evals_dir, list_suites, load_suite, load_tasks, suite_declares_generated_tasks,
    LoadedTaskFile,
};
pub use multi_judge::{
    AggregatedResult, AggregatedVerdict, AggregationMode, JudgeConfig, JudgeResult,
    MultiJudgeConfig, MultiJudgeScorer,
};
pub use optimizer::{
    generate_candidates, parse_cadence, AppliedPatch, OptimizerRunParams, OptimizerRunReport,
    OptimizerRunStatus, OptimizerRuntime, RejectedPatch, RollbackReport, ScalarCandidate,
    ScalarOptimizer,
};
pub use pending_badcase::{
    dedup_hash, InsertPendingParams, PendingBadcase, PendingBadcaseStore, PendingSource,
    PendingStatus,
};
pub use proposer::{
    fence_path, AdoptionReport, StructuralCandidate, StructuralObjectKind, StructuralProposer,
};
pub use rca::{
    module_to_owner, BadcaseEntry, CandidateModule, ModuleVerdict, ProblemPhenomenon, RcaInput,
    RcaKnowledgeBase, RcaKnowledgeBaseEntry, RcaPipeline, RcaResult,
};
pub use recycle::{
    extract_rca_results_from_badcases, load_badcase_suite, load_governed_badcase_suite,
    BadcaseCluster, BadcaseCollector, BadcaseFixStatus, BadcaseGovernance, BadcaseRecord,
};
pub use sample_store::{InsertSampleParams, SampleVerdict, TurnSample, TurnSampleStore};
pub use scorer::{
    LayeredScorer, RiskSignalChecker, RiskTurnInput, ScorerConfig, ScoringOutput, ScreeningLayer,
    Verdict,
};
pub use skill_scorer::{
    ExecutionCheckResult, QualityCheckResult, ResilienceCheckResult, SkillCheckResult, SkillScorer,
    TriggerCheckResult,
};
pub use verdict::{
    CandidateVerdict, CandidateVerifier, HarnessCandidateVerifier, NoopVerifier, VerdictSubject,
};
