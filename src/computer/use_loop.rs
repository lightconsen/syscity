//! Computer Use loop — the canonical screenshot → decide → execute → verify
//! cycle.
//!
//! This module implements the standard Anthropic Computer Use interaction
//! pattern without tying itself to any specific LLM provider.  The caller
//! supplies a `decide` closure that receives the current [`LoopState`] and
//! returns what to do next.
//!
//! ```text
//! while not done:
//!     screenshot = adapter.screenshot()          // perceive
//!     decision   = decide(state)                 // plan
//!     result     = adapter.execute(action)       // act
//!     ok         = verifier.verify(criteria)     // validate
//! ```

use std::sync::Arc;

use crate::computer::session::{ComputerSession, SessionConfig};
use crate::computer::{
    ActionResult, ComputerAdapter, DesktopAction, Result, Screenshot, VerificationConfig,
};

/// Configuration for the Computer Use loop.
#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    /// Maximum number of steps before giving up.
    pub max_steps: usize,
    /// Delay after executing an action before verification (ms).
    pub settle_delay_ms: u64,
    /// Whether to run verification after every action.
    pub verify_after_each: bool,
    /// Verification configuration (retries, delay, etc.).
    pub verification: VerificationConfig,
    /// Region to screenshot each iteration (`None` = full screen).
    pub screenshot_region: Option<crate::computer::Rect>,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 30,
            settle_delay_ms: 500,
            verify_after_each: true,
            verification: VerificationConfig::default(),
            screenshot_region: None,
        }
    }
}

/// What the decision maker wants the loop to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopDecision {
    /// Execute a desktop action.
    Action(DesktopAction),
    /// Task is complete.
    Done { message: String },
    /// The agent is stuck and needs human help.
    NeedHelp { reason: String },
}

/// A single step that was executed.
#[derive(Debug, Clone)]
pub struct StepRecord {
    pub step: usize,
    pub action: DesktopAction,
    pub result: ActionResult,
    pub verification: StepVerification,
    pub screenshot_before: Option<Screenshot>,
    pub screenshot_after: Option<Screenshot>,
    /// Number of snapshots taken before this step (for undo / rollback).
    pub snapshots_taken: usize,
}

/// Current state exposed to the decision maker.
#[derive(Debug, Clone)]
pub struct LoopState {
    /// Which step we are about to execute (0-indexed).
    pub step: usize,
    /// The original goal.
    pub goal: String,
    /// The most recent screenshot.
    pub screenshot: Screenshot,
    /// All previously executed steps.
    pub history: Vec<StepRecord>,
    /// Whether the previous step verified successfully.
    pub last_verified: Option<bool>,
    /// Error message from the previous step, if any.
    pub last_error: Option<String>,
    /// Number of consecutive failed steps.
    pub consecutive_failures: usize,
}

/// What a verification established about an action.
///
/// Three states, because two cannot hold the truth: a check that could not run
/// is neither a pass nor a failure, and folding it into either invents a fact.
/// `verify_by_diff` used to fold it into a pass — it answered `true` when it had
/// no screenshots to compare, so "could not tell" and "it worked" were the same
/// answer downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepVerification {
    /// The action's effect was observed.
    Verified,
    /// The action did not have the effect it should have had.
    Failed,
    /// Nothing was established: the check could not run, or no check exists for
    /// this action.
    Unverifiable,
}

impl StepVerification {
    /// A `VerificationEngine` answer, whose false means the criterion was
    /// checked and not met — never "unknown".
    pub fn from_checked(verified: bool) -> Self {
        if verified {
            StepVerification::Verified
        } else {
            StepVerification::Failed
        }
    }

    /// Whether this step was actually verified.
    ///
    /// Both `Failed` and `Unverifiable` answer false here, which is the right
    /// answer to "was it verified?" — but they are not the same thing, and the
    /// enum is what keeps them apart.
    pub fn verified(self) -> bool {
        matches!(self, StepVerification::Verified)
    }
}

/// Outcome of running the loop.
#[derive(Debug, Clone)]
pub struct LoopResult {
    pub success: bool,
    pub steps_taken: usize,
    pub history: Vec<StepRecord>,
    pub final_screenshot: Screenshot,
    pub message: String,
}

/// The canonical Computer Use orchestrator.
pub struct ComputerUseLoop {
    adapter: Arc<dyn ComputerAdapter>,
    config: LoopConfig,
    /// Verification / rollback / settle / memory settings, handed to the
    /// session that runs each step.
    session_config: SessionConfig,
    rollback_manager: Option<Arc<tokio::sync::Mutex<crate::computer::RollbackManager>>>,
    /// Optional execution controller for pause / resume / step / cancel.
    execution_controller: Option<Arc<crate::acp::ExecutionController>>,
    /// The step engine. Shared with the `computer` tool's code path, so the
    /// loop and the tool cannot drift apart in what "verified" means.
    session: Arc<ComputerSession>,
}

impl ComputerUseLoop {
    pub fn new(adapter: Arc<dyn ComputerAdapter>) -> Self {
        let session_config = SessionConfig::default();
        let session =
            Arc::new(ComputerSession::new(adapter.clone()).with_config(session_config.clone()));
        Self {
            adapter,
            config: LoopConfig::default(),
            session_config,
            rollback_manager: None,
            execution_controller: None,
            session,
        }
    }

    /// Rebuild the step engine after a setting changed.
    fn rebuild_session(&mut self) {
        let mut session =
            ComputerSession::new(self.adapter.clone()).with_config(self.session_config.clone());
        if let Some(mgr) = self.rollback_manager.clone() {
            session = session.with_rollback_manager(mgr);
        }
        self.session = Arc::new(session);
    }

    pub fn with_config(mut self, config: LoopConfig) -> Self {
        self.config = config;
        self.session_config.verification = config.verification;
        self.session_config.settle.initial_delay_ms = config.settle_delay_ms;
        self.session_config.verify = config.verify_after_each;
        self.rebuild_session();
        self
    }

    /// Attach an execution controller for pause / resume / step / cancel.
    pub fn with_execution_controller(
        mut self,
        controller: Arc<crate::acp::ExecutionController>,
    ) -> Self {
        self.execution_controller = Some(controller);
        self
    }

    /// Attach a rollback manager for auto-rollback and step-by-step undo.
    pub fn with_rollback_manager(
        mut self,
        manager: Arc<tokio::sync::Mutex<crate::computer::RollbackManager>>,
    ) -> Self {
        self.rollback_manager = Some(manager);
        self.rebuild_session();
        self
    }

    /// Paths to snapshot before each action (directories or files).
    pub fn with_snapshot_paths(mut self, paths: Vec<String>) -> Self {
        self.session_config.snapshot_paths = paths;
        self.rebuild_session();
        self
    }

    /// Set the consecutive-failure threshold for auto-rollback.
    /// Default is 3.
    pub fn with_auto_rollback_threshold(mut self, threshold: usize) -> Self {
        self.session_config.auto_rollback_threshold = threshold;
        self.rebuild_session();
        self
    }

    /// Undo the last `n` steps by rolling back their snapshots.
    ///
    /// Returns the number of steps actually undone.
    pub async fn undo_steps(&self, n: usize) -> Result<usize> {
        self.session.undo_steps(n).await
    }

    /// Undo the most recent step.
    pub async fn undo_last_step(&self) -> Result<bool> {
        self.session.undo_last_step().await
    }

    /// Run the Computer Use loop.
    ///
    /// `decide` is called every iteration with the current [`LoopState`].
    /// It must return a [`LoopDecision`] telling the loop what to do next.
    pub async fn run<F, Fut>(&self, goal: &str, mut decide: F) -> Result<LoopResult>
    where
        F: FnMut(LoopState) -> Fut,
        Fut: std::future::Future<Output = Result<LoopDecision>>,
    {
        let mut history = Vec::new();
        let mut last_verified: Option<bool> = None;
        let mut last_error: Option<String> = None;
        let mut consecutive_failures: usize = 0;

        // A run is a task: failure counters and memory start clean.
        self.session.reset().await;

        for step in 0..self.config.max_steps {
            // Check execution controller for pause / resume / step / cancel.
            if let Some(ref ctrl) = self.execution_controller {
                if let Err(reason) = ctrl.check_and_wait().await {
                    let final_screenshot = self.fallback_screenshot().await;
                    return Ok(LoopResult {
                        success: false,
                        steps_taken: step,
                        history,
                        final_screenshot,
                        message: reason.to_string(),
                    });
                }
            }

            // Auto-escalation: too many consecutive failures.
            if consecutive_failures >= 5 {
                let final_screenshot = self.fallback_screenshot().await;
                return Ok(LoopResult {
                    success: false,
                    steps_taken: step,
                    history,
                    final_screenshot,
                    message: format!(
                        "Stuck after {} consecutive failures. Last error: {}",
                        consecutive_failures,
                        last_error.as_deref().unwrap_or("unknown")
                    ),
                });
            }

            // 1. Perceive — capture screenshot (fallback on headless).
            let screenshot = self.fallback_screenshot().await;

            // 2. Plan — ask the decision maker what to do.
            let state = LoopState {
                step,
                goal: goal.to_string(),
                screenshot: screenshot.clone(),
                history: history.clone(),
                last_verified,
                last_error: last_error.clone(),
                consecutive_failures,
            };

            let decision = decide(state).await?;

            match decision {
                LoopDecision::Done { message } => {
                    return Ok(LoopResult {
                        success: true,
                        steps_taken: step,
                        history,
                        final_screenshot: screenshot,
                        message,
                    });
                }
                LoopDecision::NeedHelp { reason } => {
                    return Ok(LoopResult {
                        success: false,
                        steps_taken: step,
                        history,
                        final_screenshot: screenshot,
                        message: reason,
                    });
                }
                LoopDecision::Action(action) => {
                    // Act → settle → verify, in the session. Snapshots before
                    // the step, three-state verification after it, failure
                    // accounting, auto-rollback and reflection all happen
                    // there — the loop only decides what to do next.
                    match self.session.execute(action.clone()).await {
                        Ok(outcome) => {
                            let verification = outcome.verification;
                            consecutive_failures = self.session.consecutive_failures().await;
                            last_error = self.session.last_error().await;
                            last_verified = match verification {
                                StepVerification::Verified => Some(true),
                                StepVerification::Failed => Some(false),
                                StepVerification::Unverifiable => None,
                            };
                            if outcome.rolled_back {
                                tracing::warn!("step rolled back automatically");
                            }

                            history.push(StepRecord {
                                step,
                                action,
                                result: outcome.result,
                                verification,
                                // The session keeps the pre-action capture for
                                // its own diff; the record carries what the
                                // caller may want to look at afterwards.
                                screenshot_before: None,
                                screenshot_after: outcome.state_after.map(|state| state.screenshot),
                                snapshots_taken: outcome.snapshots_taken,
                            });
                        }
                        Err(e) => {
                            consecutive_failures = self.session.record_error(&e.to_string()).await;
                            last_verified = Some(false);
                            last_error = Some(e.to_string());

                            history.push(StepRecord {
                                step,
                                action,
                                result: ActionResult::error(e.to_string()),
                                // The action itself errored: that is a failure,
                                // not an absent check.
                                verification: StepVerification::Failed,
                                screenshot_before: None,
                                screenshot_after: None,
                                snapshots_taken: 0,
                            });
                        }
                    }
                }
            }
        }

        // Max steps reached.
        let final_screenshot = self.fallback_screenshot().await;
        Ok(LoopResult {
            success: false,
            steps_taken: history.len(),
            history,
            final_screenshot,
            message: format!("Max steps ({}) reached", self.config.max_steps),
        })
    }

    /// Take a screenshot, falling back to an empty placeholder when the
    /// display is not available (e.g. headless / CI).
    async fn fallback_screenshot(&self) -> Screenshot {
        self.adapter
            .screenshot(self.config.screenshot_region)
            .await
            .unwrap_or_else(|_| Screenshot::new(String::new(), 0, 0))
    }
}

#[cfg(test)]
mod tests {
    // The verification rules that used to live here now live in
    // `computer::session`, which the loop delegates to. Their tests moved
    // with them.

    use super::*;

    #[test]
    fn test_loop_config_default() {
        let cfg = LoopConfig::default();
        assert_eq!(cfg.max_steps, 30);
        assert!(cfg.verify_after_each);
    }

    #[test]
    fn test_loop_decision_serde() {
        let d = LoopDecision::Done {
            message: "finished".to_string(),
        };
        // LoopDecision does not derive Serialize, but we can at least clone it.
        let _cloned = d.clone();
        assert_eq!(
            d,
            LoopDecision::Done {
                message: "finished".to_string()
            }
        );
    }

    #[test]
    fn test_loop_state_creation() {
        let state = LoopState {
            step: 0,
            goal: "test".to_string(),
            screenshot: Screenshot::new(String::new(), 100, 100),
            history: vec![],
            last_verified: None,
            last_error: None,
            consecutive_failures: 0,
        };
        assert_eq!(state.step, 0);
        assert!(state.history.is_empty());
        assert_eq!(state.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_execution_controller_cancels_loop() {
        // Verify that attaching an already-cancelled controller aborts the loop
        // immediately with a cancellation message.
        let adapter = Arc::new(crate::computer::headless::HeadlessComputerAdapter::new());
        let ctrl = crate::acp::ExecutionController::new();
        ctrl.cancel().await;

        let loop_ = ComputerUseLoop::new(adapter).with_execution_controller(ctrl);

        let result = loop_
            .run("test goal", |_state| async {
                Ok(LoopDecision::Done {
                    message: "should not reach".to_string(),
                })
            })
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(!result.success);
        assert_eq!(result.steps_taken, 0);
        assert!(result.message.contains("cancelled"));
    }

    #[tokio::test]
    async fn test_undo_steps_with_rollback_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("workspace.txt");
        tokio::fs::write(&file, "original").await.unwrap();

        let mgr = Arc::new(tokio::sync::Mutex::new(
            crate::computer::RollbackManager::with_backup_dir(tmp.path().join("backups")),
        ));

        let loop_ = ComputerUseLoop::new(Arc::new(
            crate::computer::headless::HeadlessComputerAdapter::new(),
        ))
        .with_rollback_manager(mgr.clone())
        .with_snapshot_paths(vec![file.to_string_lossy().to_string()]);

        // Create real snapshots in the manager (simulating what run() would do).
        {
            let mut m = mgr.lock().await;
            m.snapshot_file(&file).await.unwrap();
        }
        tokio::fs::write(&file, "after-step-0").await.unwrap();

        {
            let mut m = mgr.lock().await;
            m.snapshot_file(&file).await.unwrap();
        }
        tokio::fs::write(&file, "after-step-1").await.unwrap();

        // Simulate two steps, each taking one snapshot.
        loop_.session.push_snapshot_count(1).await; // step 0
        loop_.session.push_snapshot_count(1).await; // step 1

        let undone = loop_.undo_steps(1).await.unwrap();
        assert_eq!(undone, 1);

        // Only the last snapshot should have been restored
        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(content, "after-step-0");

        // Undo the remaining step
        let undone = loop_.undo_steps(1).await.unwrap();
        assert_eq!(undone, 1);

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(content, "original");
    }

    #[tokio::test]
    async fn test_undo_last_step_no_manager_returns_false() {
        let loop_ = ComputerUseLoop::new(Arc::new(
            crate::computer::headless::HeadlessComputerAdapter::new(),
        ));

        let undone = loop_.undo_last_step().await.unwrap();
        assert!(!undone);
    }

    #[test]
    fn test_computer_use_loop_builder_methods() {
        let adapter = Arc::new(crate::computer::headless::HeadlessComputerAdapter::new());
        let mgr = Arc::new(tokio::sync::Mutex::new(
            crate::computer::RollbackManager::with_backup_dir(std::env::temp_dir().join("test")),
        ));

        let loop_ = ComputerUseLoop::new(adapter)
            .with_rollback_manager(mgr)
            .with_snapshot_paths(vec!["/tmp/workspace".to_string()])
            .with_auto_rollback_threshold(5);

        assert_eq!(loop_.session_config.snapshot_paths.len(), 1);
        assert_eq!(loop_.session_config.auto_rollback_threshold, 5);
        assert!(loop_.rollback_manager.is_some());
    }
}
