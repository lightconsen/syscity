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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::computer::{
    ActionResult, ComputerAdapter, ComputerError, DesktopAction, Result, Screenshot, UiElement,
    VerificationConfig, VerificationCriteria, VerificationEngine,
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
    fn from_checked(verified: bool) -> Self {
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
    verifier: VerificationEngine,
    config: LoopConfig,
    /// Optional execution controller for pause / resume / step / cancel.
    execution_controller: Option<Arc<crate::acp::ExecutionController>>,
    /// Optional rollback manager for auto-rollback and step-by-step undo.
    rollback_manager: Option<Arc<tokio::sync::Mutex<crate::computer::RollbackManager>>>,
    /// Paths to snapshot before each action (workspace dirs, config files,
    /// etc.).
    snapshot_paths: Vec<String>,
    /// Consecutive-failure threshold that triggers auto-rollback.
    auto_rollback_threshold: usize,
    /// Guards against repeatedly triggering auto-rollback between the
    /// threshold and escalation (set true on first rollback, reset on
    /// success).
    rollback_already_triggered: AtomicBool,
    /// Tracks how many snapshots were taken for each step (index = step
    /// number).
    step_snapshot_counts: Arc<tokio::sync::Mutex<Vec<usize>>>,
}

impl ComputerUseLoop {
    pub fn new(adapter: Arc<dyn ComputerAdapter>) -> Self {
        let verifier =
            VerificationEngine::new(adapter.clone()).with_config(VerificationConfig::default());
        Self {
            adapter,
            verifier,
            config: LoopConfig::default(),
            execution_controller: None,
            rollback_manager: None,
            snapshot_paths: Vec::new(),
            auto_rollback_threshold: 3,
            rollback_already_triggered: AtomicBool::new(false),
            step_snapshot_counts: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn with_config(mut self, config: LoopConfig) -> Self {
        self.verifier =
            VerificationEngine::new(self.adapter.clone()).with_config(config.verification);
        self.config = config;
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
        self
    }

    /// Paths to snapshot before each action (directories or files).
    pub fn with_snapshot_paths(mut self, paths: Vec<String>) -> Self {
        self.snapshot_paths = paths;
        self
    }

    /// Set the consecutive-failure threshold for auto-rollback.
    /// Default is 3.
    pub fn with_auto_rollback_threshold(mut self, threshold: usize) -> Self {
        self.auto_rollback_threshold = threshold;
        self
    }

    /// Undo the last `n` steps by rolling back their snapshots.
    ///
    /// Returns the number of steps actually undone.
    pub async fn undo_steps(&self, n: usize) -> Result<usize> {
        let counts = self.step_snapshot_counts.lock().await;
        let n = n.min(counts.len());
        if n == 0 {
            return Ok(0);
        }
        let total_snapshots: usize = counts[counts.len().saturating_sub(n)..].iter().sum();
        drop(counts);

        if let Some(ref mgr) = self.rollback_manager {
            let mut mgr = mgr.lock().await;
            mgr.rollback_last(total_snapshots)
                .await
                .map_err(|e| ComputerError::Other(format!("Rollback failed: {}", e)))?;
        }

        let mut counts = self.step_snapshot_counts.lock().await;
        let new_len = counts.len().saturating_sub(n);
        counts.truncate(new_len);

        Ok(n)
    }

    /// Undo the most recent step.
    pub async fn undo_last_step(&self) -> Result<bool> {
        self.undo_steps(1).await.map(|n| n > 0)
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
        let mut current_settle_delay_ms = self.config.settle_delay_ms;

        // Reset per-run snapshot tracking.
        self.step_snapshot_counts.lock().await.clear();

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

            // Auto-rollback: trigger exactly once when threshold is reached.
            if consecutive_failures == self.auto_rollback_threshold
                && !self.rollback_already_triggered.load(Ordering::Relaxed)
            {
                self.rollback_already_triggered
                    .store(true, Ordering::Relaxed);
                tracing::warn!(
                    "Auto-rollback triggered after {} consecutive failures",
                    consecutive_failures
                );
                if let Some(ref mgr) = self.rollback_manager {
                    let mut mgr = mgr.lock().await;
                    if let Err(e) = mgr.rollback().await {
                        tracing::error!("Auto-rollback failed: {}", e);
                    } else {
                        tracing::info!("Auto-rollback completed successfully");
                    }
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
                    // 3. Snapshot configured paths before acting.
                    let mut snapshots_taken = 0;
                    if !self.snapshot_paths.is_empty() {
                        if let Some(ref mgr) = self.rollback_manager {
                            let mut mgr = mgr.lock().await;
                            for path_str in &self.snapshot_paths {
                                let path = std::path::Path::new(path_str);
                                let snap_result = if tokio::fs::metadata(path)
                                    .await
                                    .map(|m| m.is_dir())
                                    .unwrap_or(false)
                                {
                                    mgr.snapshot_directory(path).await
                                } else {
                                    mgr.snapshot_file(path).await
                                };
                                if let Err(e) = snap_result {
                                    tracing::warn!(
                                        "Failed to snapshot '{}' before step {}: {}",
                                        path_str,
                                        step,
                                        e
                                    );
                                } else {
                                    snapshots_taken += 1;
                                }
                            }
                        }
                    }
                    self.step_snapshot_counts.lock().await.push(snapshots_taken);

                    // 4. Act — execute the action.
                    let screenshot_before = Some(screenshot);
                    // Capture the pre-action UI tree for diff-based
                    // verification of screen-mutating actions.
                    let tree_before = if crate::computer::vision::is_screen_mutating_action(&action)
                    {
                        self.adapter.read_ui_tree(None).await.ok()
                    } else {
                        None
                    };
                    let result = self.adapter.execute(action.clone()).await;

                    match result {
                        Ok(result) => {
                            // 5. Validate — verify the outcome.
                            tokio::time::sleep(Duration::from_millis(current_settle_delay_ms))
                                .await;

                            let screenshot_after = self
                                .adapter
                                .screenshot(self.config.screenshot_region)
                                .await
                                .ok();

                            let verification = if self.config.verify_after_each {
                                self.verify_action(
                                    &action,
                                    &result,
                                    tree_before.as_deref(),
                                    screenshot_before.as_ref(),
                                    screenshot_after.as_ref(),
                                )
                                .await
                                // A check that errored established nothing. It
                                // is not evidence that the action failed.
                                .unwrap_or(StepVerification::Unverifiable)
                            } else {
                                StepVerification::Unverifiable
                            };

                            match verification {
                                StepVerification::Verified => {
                                    consecutive_failures = 0;
                                    self.rollback_already_triggered
                                        .store(false, Ordering::Relaxed);
                                    current_settle_delay_ms = self.config.settle_delay_ms;
                                    last_error = None;
                                }
                                StepVerification::Failed => {
                                    consecutive_failures += 1;
                                    last_error = Some("Verification failed".to_string());
                                }
                                // Neither counter moves: counting it as a
                                // failure would roll back work that may be
                                // fine, and counting it as a success would
                                // reset the failures that were real. The
                                // previous error, if any, stands.
                                StepVerification::Unverifiable => {
                                    tracing::info!(
                                        "verification: nothing established for this step"
                                    );
                                }
                            }
                            last_verified = match verification {
                                StepVerification::Verified => Some(true),
                                StepVerification::Failed => Some(false),
                                StepVerification::Unverifiable => None,
                            };

                            history.push(StepRecord {
                                step,
                                action,
                                result,
                                verification,
                                screenshot_before,
                                screenshot_after,
                                snapshots_taken,
                            });
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            last_verified = Some(false);
                            last_error = Some(e.to_string());

                            // Adaptive settle delay: if multiple consecutive failures look
                            // like timing issues, wait longer before next action.
                            if consecutive_failures >= 3 {
                                let err_lower = e.to_string().to_lowercase();
                                let is_timing = err_lower.contains("timeout")
                                    || err_lower.contains("not ready")
                                    || err_lower.contains("not found")
                                    || err_lower.contains("empty");
                                if is_timing {
                                    let new_delay =
                                        current_settle_delay_ms.saturating_mul(2).min(5000);
                                    if new_delay > current_settle_delay_ms {
                                        tracing::warn!(
                                            "Increasing settle delay from {}ms to {}ms after {} \
                                             consecutive timing failures",
                                            current_settle_delay_ms,
                                            new_delay,
                                            consecutive_failures
                                        );
                                        current_settle_delay_ms = new_delay;
                                    }
                                }
                            }

                            history.push(StepRecord {
                                step,
                                action,
                                result: ActionResult::error(e.to_string()),
                                // The action itself errored: that is a failure, not
                                // an absent check.
                                verification: StepVerification::Failed,
                                screenshot_before,
                                screenshot_after: None,
                                snapshots_taken,
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

    /// Verify a single action.
    ///
    /// Actions with targeted criteria (LaunchApp, ActivateWindow, KillProcess)
    /// use the [`VerificationEngine`]; other screen-mutating actions are
    /// verified by diffing the pre/post [`ScreenState`] — if nothing visible
    /// changed, the action likely did not land.
    async fn verify_action(
        &self,
        action: &DesktopAction,
        _result: &ActionResult,
        tree_before: Option<&[UiElement]>,
        screenshot_before: Option<&Screenshot>,
        screenshot_after: Option<&Screenshot>,
    ) -> Result<StepVerification> {
        match action {
            DesktopAction::Screenshot { .. } => {
                // A screenshot is the thing the action was for.
                Ok(StepVerification::Verified)
            }
            DesktopAction::LaunchApp { name, wait_for_ready, .. } => {
                if *wait_for_ready {
                    self.verifier
                        .verify(
                            &VerificationCriteria::ProcessRunning { name: name.clone() },
                            &ActionResult::success(""),
                            None,
                        )
                        .await
                        .map(StepVerification::from_checked)
                } else {
                    // The caller asked not to wait for it, so nothing was
                    // checked.
                    Ok(StepVerification::Unverifiable)
                }
            }
            DesktopAction::Wait { .. } => Ok(StepVerification::Verified),
            DesktopAction::ActivateWindow { title_pattern } => self
                .verifier
                .verify(
                    &VerificationCriteria::WindowTitleContains { pattern: title_pattern.clone() },
                    &ActionResult::success(""),
                    None,
                )
                .await
                .map(StepVerification::from_checked),
            _ if crate::computer::vision::is_screen_mutating_action(action) => {
                self.verify_by_diff(tree_before, screenshot_before, screenshot_after)
                    .await
            }
            // No check exists for this action. Saying so is the only honest
            // option: the previous `Ok(true)` here was a claim of success that
            // nothing had established.
            _ => Ok(StepVerification::Unverifiable),
        }
    }

    /// Verify by diffing pre/post screen state: pixel diff on the screenshots
    /// plus tree diff on the accessibility trees. Any meaningful visible
    /// change counts as verified.
    async fn verify_by_diff(
        &self,
        tree_before: Option<&[UiElement]>,
        screenshot_before: Option<&Screenshot>,
        screenshot_after: Option<&Screenshot>,
    ) -> Result<StepVerification> {
        let (Some(shot_before), Some(shot_after)) = (screenshot_before, screenshot_after) else {
            // No screenshots to compare (e.g. headless). That is not a pass:
            // nothing was checked, and claiming otherwise is what this enum
            // exists to stop.
            return Ok(StepVerification::Unverifiable);
        };
        if shot_before.base64.is_empty() || shot_after.base64.is_empty() {
            return Ok(StepVerification::Unverifiable);
        }

        let tree_after = self.adapter.read_ui_tree(None).await.unwrap_or_default();
        let before = crate::computer::vision::ScreenState::from_parts(
            tree_before.unwrap_or(&[]).to_vec(),
            shot_before.clone(),
        );
        let after =
            crate::computer::vision::ScreenState::from_parts(tree_after, shot_after.clone());
        let diff = before.diff(&after);
        let changed = diff.has_meaningful_changes();
        if !changed {
            tracing::info!("verification: no visible change ({})", diff.summary());
        }
        Ok(if changed {
            StepVerification::Verified
        } else {
            StepVerification::Failed
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
    #[tokio::test]
    async fn a_check_that_cannot_run_establishes_nothing() {
        // The bug this replaces: `verify_by_diff` answered `true` when it had no
        // screenshots to compare, so "could not tell" and "it worked" were the
        // same answer, and the loop reset its failure counter on both.
        let loop_ = ComputerUseLoop::new(Arc::new(
            crate::computer::headless::HeadlessComputerAdapter::new(),
        ));

        let outcome = loop_.verify_by_diff(None, None, None).await.unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
        assert!(!outcome.verified(), "unverifiable is not verified");

        // An empty screenshot is the same situation: nothing to compare.
        let empty = Screenshot::new(String::new(), 0, 0);
        let outcome = loop_
            .verify_by_diff(None, Some(&empty), Some(&empty))
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
    }

    #[tokio::test]
    async fn an_action_with_no_check_is_unverifiable_not_successful() {
        // `_ => Ok(true), // Other actions: assume success.` was a claim of
        // success that nothing had established.
        let loop_ = ComputerUseLoop::new(Arc::new(
            crate::computer::headless::HeadlessComputerAdapter::new(),
        ));
        let outcome = loop_
            .verify_action(
                &crate::computer::types::DesktopAction::GetSystemStatus,
                &crate::computer::types::ActionResult::success(""),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
    }

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
        {
            let mut counts = loop_.step_snapshot_counts.lock().await;
            counts.push(1); // step 0: 1 snapshot
            counts.push(1); // step 1: 1 snapshot
        }

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

        assert_eq!(loop_.snapshot_paths.len(), 1);
        assert_eq!(loop_.auto_rollback_threshold, 5);
        assert!(loop_.rollback_manager.is_some());
    }
}
