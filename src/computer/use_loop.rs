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
    /// Execute several actions, in order, against the same look.
    ///
    /// This is the batched form of [`Self::Action`], and it exists because a
    /// person filling a form looks once and acts four times (`click → type →
    /// Tab → type`): every action in the batch follows from the picture the
    /// decision was made against. Running them without a capture between is
    /// what turns a twenty-field form from twenty round trips into a handful.
    ///
    /// The loop still records one step per action, settles and verifies each,
    /// and stops the batch at the first action that fails verification or is
    /// refused — the actions after it were planned on a screen that did not
    /// come to pass.
    Batch(Vec<DesktopAction>),
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
    /// Whether the step aimed at an element the sensitive-UI detector flagged.
    ///
    /// Carried on the record rather than only inside the session because a
    /// resumed run rebuilds its history from the checkpoint, and a person
    /// approving that resume is entitled to know which steps touched a
    /// password field or a payment form.
    pub sensitive: bool,
    /// The box a click-like action aimed at, when the visible change landed
    /// somewhere else.
    ///
    /// `Some` only for the case the three-state verdict calls `Unverifiable`
    /// for a reason worth repeating: the screen *did* change, just not where
    /// the action pointed. The diff that established this is dropped when the
    /// step ends, and the run's next decision prompt renders one line per past
    /// step — so without this box, "unchecked" is all a later prompt can say
    /// about a click that visibly missed. A model told only "unchecked" tends
    /// to retry the same click; told where it aimed, it can move.
    ///
    /// `None` for every other step: a change at the target, a tree-corroborated
    /// one, a step that could not be compared at all, or an action that aimed
    /// nowhere (a key press, a launch). Carrying a box in those cases would
    /// say "elsewhere" about a question nobody answered.
    pub aim_missed: Option<crate::computer::Rect>,
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
    /// Why the capture is empty, when it is.
    ///
    /// A screen that could not be captured is not a screen with nothing on it.
    /// The loop used to answer both with an empty placeholder `Screenshot`, so
    /// a host whose display is blank — a remote machine with nobody logged in,
    /// a locked session — was reported to the model as an empty screen, which
    /// is a claim about the world rather than about the capture. The reason is
    /// carried here so the decision can say "I could not look", and the run
    /// stops as `NeedHelp` instead of clicking into the dark.
    pub capture_error: Option<String>,
    /// All previously executed steps.
    pub history: Vec<StepRecord>,
    /// Whether the previous step verified successfully.
    pub last_verified: Option<bool>,
    /// Error message from the previous step, if any.
    pub last_error: Option<String>,
    /// Number of consecutive failed steps.
    pub consecutive_failures: usize,
    /// The screen area this capture covers, in screen points.
    ///
    /// `None` means the whole display layout — what a normal step captures.
    /// `Some` means the model asked to look closer at one rectangle on the
    /// previous step, and this capture is that crop. It is not decoration:
    /// a crop's pixels start at the crop's corner, so anything mapping image
    /// pixels back to screen points has to know the origin, or every control
    /// it finds lands at an offset that grows with the distance from the
    /// top-left. [`CoordinateSpace::for_region`] is the mapping built from it.
    ///
    /// [`CoordinateSpace::for_region`]: crate::computer::display::CoordinateSpace::for_region
    pub capture_region: Option<crate::computer::Rect>,
}

/// What a verification established about an action.
///
/// Three states, because two cannot hold the truth: a check that could not run
/// is neither a pass nor a failure, and folding it into either invents a fact.
/// `verify_by_diff` used to fold it into a pass — it answered `true` when it had
/// no screenshots to compare, so "could not tell" and "it worked" were the same
/// answer downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// The state a resumed loop starts from.
///
/// A run cut off by a restart has already taken real actions; the loop must
/// not forget them, or the decision maker would be shown an empty history and
/// asked to do the same things again. `start_step` is absolute — the step
/// numbering continues where the previous process left it, and so does the
/// `max_steps` ceiling.
#[derive(Debug, Clone, Default)]
pub struct LoopResume {
    /// Steps the previous process completed, oldest first.
    pub history: Vec<StepRecord>,
    /// The step number to execute next.
    pub start_step: usize,
    /// Whether the step before the restart verified. Carried so the vision and
    /// image triggers keep the same view of the run.
    pub last_verified: Option<bool>,
    /// Consecutive failures at the checkpoint.
    pub consecutive_failures: usize,
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
    /// Where a resumed run starts, when it is one.
    resume: Option<LoopResume>,
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
            resume: None,
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

    /// The apps this loop may act on (`[computer] allow_apps`).
    ///
    /// The list is shared, so a config edit reaches a loop that is already
    /// running; this only has to install the handle.
    pub fn with_allow_apps(mut self, allow_apps: crate::computer::app_scope::AppAllowlist) -> Self {
        self.session_config.allow_apps = allow_apps;
        self.rebuild_session();
        self
    }

    /// The approval this loop's desktop work runs under.
    ///
    /// The adapter's inner calls require approval and run with a bare context
    /// that has no ask queue, so without this each step would be refused by the
    /// gate the caller already passed. See [`SessionConfig::approved_by`].
    pub fn with_approval_provenance(
        mut self,
        approved_by: impl Into<String>,
        conversation_id: impl Into<String>,
    ) -> Self {
        self.session_config.approved_by = approved_by.into();
        self.session_config.conversation_id = conversation_id.into();
        self.rebuild_session();
        self
    }

    /// Paths to snapshot before each action (directories or files).
    pub fn with_snapshot_paths(mut self, paths: Vec<String>) -> Self {
        self.session_config.snapshot_paths = paths;
        self.rebuild_session();
        self
    }

    /// Continue a run from where a previous process left it.
    ///
    /// The history is carried so the decision maker sees what was already
    /// done, and the step counter continues from the checkpoint — a resumed
    /// run does not get a fresh `max_steps`. Nothing already executed is
    /// replayed: the history is *memory*, not a script.
    pub fn with_resume(mut self, resume: LoopResume) -> Self {
        self.resume = Some(resume);
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
        let resume = self.resume.clone().unwrap_or_default();
        let mut history = resume.history;
        let mut last_verified: Option<bool> = resume.last_verified;
        let mut last_error: Option<String> = None;
        let mut consecutive_failures: usize = resume.consecutive_failures;

        // What the next capture should cover. The model asks for a closer look
        // by answering with a region screenshot; on the next iteration this is
        // what the loop actually captures. It is one-shot on purpose: a crop
        // the model asked for answers *that* step's question, and the step
        // after it should see the whole screen again unless it asks again.
        // Otherwise a single request for a close-up would narrow every later
        // decision to a rectangle it never chose.
        let mut capture_region: Option<crate::computer::Rect> = None;

        // A run is a task: failure counters and memory start clean — unless
        // this *is* a continued task, in which case the counters are the
        // previous process's.
        //
        // The session itself always starts clean: it holds per-step scratch
        // state (screenshots, snapshots, the failure streak), none of which
        // survives a process, and rebuilding it from a checkpoint would be
        // inventing facts about a screen that may have changed.
        self.session.reset().await;

        let mut step = resume.start_step;
        while step < self.config.max_steps {
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

            // 1. Perceive — capture screenshot. A capture that failed is not
            //    an empty screen: carry the reason and let the decision maker
            //    answer "I cannot look" instead of planning on a blank frame.
            let (screenshot, capture_error) = match self.adapter.screenshot(capture_region).await {
                Ok(shot) => (shot, None),
                Err(e) => {
                    tracing::warn!("screen capture failed: {e}");
                    (Screenshot::new(String::new(), 0, 0), Some(e.to_string()))
                }
            };

            // 2. Plan — ask the decision maker what to do.
            let state = LoopState {
                step,
                goal: goal.to_string(),
                screenshot: screenshot.clone(),
                capture_error: capture_error.clone(),
                history: history.clone(),
                last_verified,
                last_error: last_error.clone(),
                consecutive_failures,
                capture_region,
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
                    let ran = self
                        .run_actions(
                            &mut history,
                            vec![action],
                            step,
                            &mut consecutive_failures,
                            &mut last_verified,
                            &mut last_error,
                            &mut capture_region,
                            &screenshot,
                        )
                        .await;
                    step += ran.max(1);
                    if last_verified == Some(false) && consecutive_failures >= 5 {
                        // The batch's failure reached the escalation ceiling;
                        // stop here rather than looping once more.
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
                }
                LoopDecision::Batch(actions) => {
                    let ran = self
                        .run_actions(
                            &mut history,
                            actions,
                            step,
                            &mut consecutive_failures,
                            &mut last_verified,
                            &mut last_error,
                            &mut capture_region,
                            &screenshot,
                        )
                        .await;
                    step += ran.max(1);
                    if last_verified == Some(false) && consecutive_failures >= 5 {
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

    /// Run one step's worth of actions, recording each as its own step.
    ///
    /// The first actions of a batch run unobserved — dispatched and accounted,
    /// with no settle-and-capture between them — because that is the cost the
    /// batch exists to remove. The last one is observed as any single action
    /// is, so the next decision sees where the batch really ended up.
    ///
    /// The batch stops at the first action that fails. The rest were planned
    /// on top of it; running them would be acting on a screen that did not
    /// come to pass. A `Screenshot` action is a request for the *next*
    /// capture, not something to dispatch, so it ends the batch the way it
    /// ends a single-action step: the loop takes the picture next iteration.
    ///
    /// Every action gets its own `StepRecord` and its own number. A batch is
    /// one look with several actions, not one step: the record still has to
    /// show what each action did, and undo counts actions.
    #[allow(clippy::too_many_arguments)]
    async fn run_actions(
        &self,
        history: &mut Vec<StepRecord>,
        actions: Vec<DesktopAction>,
        step: usize,
        consecutive_failures: &mut usize,
        last_verified: &mut Option<bool>,
        last_error: &mut Option<String>,
        capture_region: &mut Option<crate::computer::Rect>,
        screenshot: &Screenshot,
    ) -> usize {
        // `step` is the loop's counter and only moves once per iteration; a
        // batch's actions are numbered from it so the history stays sorted.
        let total = actions.len();
        let mut ran = 0usize;
        for (index, action) in actions.into_iter().enumerate() {
            let record_step = step + ran;
            ran += 1;
            let is_last = index + 1 == total;

            // A screenshot is a request for the *next* capture, not an action
            // to dispatch: capturing it inside the session and then also
            // capturing at the top of the next iteration takes two pictures,
            // and the session's copy is thrown away. The loop owns the
            // capture, so it records what the model asked to see and lets the
            // next iteration take it. It also ends the batch: the actions
            // after it were planned on a picture that no longer describes the
            // screen.
            if let DesktopAction::Screenshot { region } = &action {
                *capture_region = *region;
                let coverage = match region {
                    Some(r) => format!("({}, {}) {}x{}", r.x, r.y, r.width, r.height),
                    None => "the whole screen".to_string(),
                };
                history.push(StepRecord {
                    step: record_step,
                    action: action.clone(),
                    result: ActionResult::success(format!(
                        "the next capture will cover {coverage}; nothing was changed on the screen"
                    )),
                    verification: StepVerification::Unverifiable,
                    screenshot_before: Some(screenshot.clone()),
                    screenshot_after: None,
                    snapshots_taken: 0,
                    sensitive: false,
                    aim_missed: None,
                });
                // Asking to look is not a failure, and it is not a success
                // either. Neither counter should move, or a model that asked
                // for a close-up before acting would arrive at its first
                // action already flagged as having failed once.
                *last_verified = None;
                *last_error = None;
                break;
            }

            // Act → settle → verify, in the session. Snapshots before the
            // step, three-state verification after it, failure accounting,
            // auto-rollback and reflection all happen there — the loop only
            // decides what to do next. Only the last action of the batch is
            // observed.
            let observed = is_last || total == 1;
            match self
                .session
                .execute_in_batch(action.clone(), observed)
                .await
            {
                Ok(outcome) => {
                    let verification = outcome.verification;
                    let sensitive = outcome.sensitive_target;
                    // Read before the outcome is taken apart: the diff is the
                    // only thing that knows where the change happened, and
                    // this record outlives it.
                    let aim_missed = outcome.diff.as_ref().and_then(|d| d.aim_missed());
                    *consecutive_failures = self.session.consecutive_failures().await;
                    *last_error = self.session.last_error().await;
                    *last_verified = match verification {
                        StepVerification::Verified => Some(true),
                        StepVerification::Failed => Some(false),
                        StepVerification::Unverifiable => None,
                    };
                    if outcome.rolled_back {
                        tracing::warn!("step rolled back automatically");
                    }

                    history.push(StepRecord {
                        step: record_step,
                        action,
                        result: outcome.result,
                        verification,
                        // The frame the decision was made against. Kept on
                        // the record because the replay timeline pairs it
                        // with the post-action one; a run that dropped it
                        // could show what a step changed but never what it
                        // was looking at.
                        screenshot_before: outcome.screenshot_before,
                        screenshot_after: outcome.state_after.map(|state| state.screenshot),
                        snapshots_taken: outcome.snapshots_taken,
                        sensitive,
                        aim_missed,
                    });
                }
                Err(e) => {
                    *consecutive_failures = self.session.record_error(&e.to_string()).await;
                    *last_verified = Some(false);
                    *last_error = Some(e.to_string());

                    history.push(StepRecord {
                        step: record_step,
                        action,
                        result: ActionResult::error(e.to_string()),
                        // The action itself errored: that is a failure, not an
                        // absent check.
                        verification: StepVerification::Failed,
                        screenshot_before: None,
                        screenshot_after: None,
                        snapshots_taken: 0,
                        sensitive: false,
                        // Nothing was delivered, so nothing aimed.
                        aim_missed: None,
                    });
                }
            }

            // A failed action ends the batch: everything after it was planned
            // on the screen it was supposed to produce.
            if *last_verified == Some(false) {
                break;
            }
        }
        ran
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
            capture_error: None,
            history: vec![],
            last_verified: None,
            last_error: None,
            consecutive_failures: 0,
            capture_region: None,
        };
        assert_eq!(state.step, 0);
        assert!(state.history.is_empty());
        assert_eq!(state.consecutive_failures, 0);
    }

    /// An adapter that records what area each capture was asked for.
    ///
    /// The region the loop captures with is the whole point of a close-up, and
    /// it is invisible from the decision side: the model asks, and whether the
    /// request reached the adapter is a fact only the adapter can report.
    struct RegionRecorder {
        asked: std::sync::Mutex<Vec<Option<crate::computer::Rect>>>,
    }

    #[async_trait::async_trait]
    impl crate::computer::ComputerAdapter for RegionRecorder {
        async fn screenshot(
            &self,
            region: Option<crate::computer::Rect>,
        ) -> crate::computer::Result<Screenshot> {
            self.asked.lock().unwrap().push(region);
            Ok(Screenshot::new(String::new(), 100, 100))
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<crate::computer::UiElement>> {
            Ok(Vec::new())
        }

        async fn execute(
            &self,
            _action: crate::computer::DesktopAction,
        ) -> crate::computer::Result<crate::computer::ActionResult> {
            Ok(crate::computer::ActionResult::success("ok"))
        }

        async fn wait_for(
            &self,
            _condition: crate::computer::WaitCondition,
            _timeout: std::time::Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }
    }

    /// The model asks for a close-up, and the next capture is that close-up —
    /// once. A request is not a mode: the step after it sees the whole screen
    /// again, because the model chose the crop to answer one question and never
    /// said it wanted every later decision made through that rectangle.
    #[tokio::test]
    async fn a_region_request_narrows_exactly_the_next_capture() {
        use crate::computer::DesktopAction;

        let adapter = Arc::new(RegionRecorder {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let loop_ = ComputerUseLoop::new(adapter.clone());
        let region = crate::computer::Rect::new(100, 50, 300, 200);
        let mut step = 0usize;
        let mut seen_regions = Vec::new();

        loop_
            .run("look closer at the dialog", |state| {
                seen_regions.push(state.capture_region);
                step += 1;
                async move {
                    match step {
                        1 => Ok(LoopDecision::Action(DesktopAction::Screenshot {
                            region: Some(region),
                        })),
                        // The model has looked; now it is done. A third step
                        // would prove the crop is one-shot, but the loop has
                        // already captured by then.
                        _ => Ok(LoopDecision::Done {
                            message: "read the close-up".to_string(),
                        }),
                    }
                }
            })
            .await
            .unwrap();

        let asked = adapter.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![None, Some(region)],
            "the first capture is the whole screen, the second is the close-up"
        );
        // What the decision maker was *told* it was looking at, which is what
        // the prompt renders. The second decision is made against the close-up,
        // because that is the capture the loop took after the request.
        assert_eq!(seen_regions, vec![None, Some(region)]);
    }

    /// A whole-screen screenshot request widens the next capture back, which is
    /// how a model escapes a close-up it no longer wants.
    #[tokio::test]
    async fn a_whole_screen_request_widens_the_next_capture() {
        use crate::computer::DesktopAction;

        let adapter = Arc::new(RegionRecorder {
            asked: std::sync::Mutex::new(Vec::new()),
        });
        let loop_ = ComputerUseLoop::new(adapter.clone());
        let region = crate::computer::Rect::new(0, 0, 50, 50);
        let mut step = 0usize;

        loop_
            .run("zoom out again", |_state| {
                step += 1;
                async move {
                    match step {
                        1 => Ok(LoopDecision::Action(DesktopAction::Screenshot {
                            region: Some(region),
                        })),
                        2 => Ok(LoopDecision::Action(DesktopAction::Screenshot { region: None })),
                        _ => Ok(LoopDecision::Done {
                            message: "back to the whole screen".to_string(),
                        }),
                    }
                }
            })
            .await
            .unwrap();

        let asked = adapter.asked.lock().unwrap().clone();
        assert_eq!(asked, vec![None, Some(region), None]);
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

    /// An adapter that cannot capture must not look like an adapter that
    /// captured an empty screen. The decision maker is handed the reason, and
    /// can stop the run instead of planning against a blank frame.
    #[tokio::test]
    async fn a_failed_capture_reaches_the_decision_maker() {
        use crate::computer::ComputerError;

        /// Every capture fails; nothing else is reachable.
        struct BlindAdapter;

        #[async_trait::async_trait]
        impl crate::computer::ComputerAdapter for BlindAdapter {
            async fn screenshot(
                &self,
                _region: Option<crate::computer::Rect>,
            ) -> crate::computer::Result<Screenshot> {
                Err(ComputerError::BlankScreen("no desktop session is drawing on it".to_string()))
            }

            async fn read_ui_tree(
                &self,
                _app: Option<&str>,
            ) -> crate::computer::Result<Vec<crate::computer::UiElement>> {
                Ok(Vec::new())
            }

            async fn execute(
                &self,
                _action: crate::computer::DesktopAction,
            ) -> crate::computer::Result<crate::computer::ActionResult> {
                Err(ComputerError::NoDisplay)
            }

            async fn wait_for(
                &self,
                _condition: crate::computer::WaitCondition,
                _timeout: std::time::Duration,
            ) -> crate::computer::Result<bool> {
                Ok(true)
            }
        }

        let loop_ = ComputerUseLoop::new(Arc::new(BlindAdapter));
        let mut seen = None;

        let result = loop_
            .run("open the calculator", |state| {
                seen = state.capture_error.clone();
                async {
                    // The model says "I cannot look", which is what a real one
                    // should be told to do.
                    Ok(LoopDecision::NeedHelp {
                        reason: "the screen could not be captured".to_string(),
                    })
                }
            })
            .await
            .expect("the loop returns a result rather than panicking");

        assert!(!result.success);
        let reason = seen.expect("the decision maker was told why the capture is empty");
        assert!(
            reason.contains("no desktop session is drawing"),
            "the reason must survive to the decision: {reason}"
        );
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

    /// A miss reaches the record the next decision is built from.
    ///
    /// The session establishes "the change was not where the click pointed"
    /// and returns it on the diff. The diff dies with the step; the record is
    /// what the loop hands back. Losing the fact in between is invisible in
    /// every unit test of either half — the session still reports
    /// `Unverifiable` and the record still says `Unverifiable` — and only
    /// shows up as a model that retries the same click forever.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_miss_lands_on_the_step_record() {
        use std::sync::atomic::{AtomicBool, Ordering};

        /// Blanks until something is clicked, then paints a corner — a change
        /// that is real and is not the click.
        ///
        /// Keyed on the click rather than on a capture count: the loop takes
        /// one capture for perception and another as the step's baseline, so
        /// "paint from the second capture" would paint the baseline too and
        /// the diff would find nothing.
        struct CornerPainter {
            clicked: AtomicBool,
        }

        #[async_trait::async_trait]
        impl crate::computer::ComputerAdapter for CornerPainter {
            async fn screenshot(
                &self,
                _region: Option<crate::computer::Rect>,
            ) -> crate::computer::Result<Screenshot> {
                use base64::Engine as _;
                let mut img = image::RgbImage::new(200, 200);
                if self.clicked.load(Ordering::SeqCst) {
                    for y in 150..182 {
                        for x in 150..182 {
                            img.put_pixel(x, y, image::Rgb([255, 255, 255]));
                        }
                    }
                }
                let mut buf = std::io::Cursor::new(Vec::new());
                image::DynamicImage::ImageRgb8(img)
                    .write_to(&mut buf, image::ImageFormat::Png)
                    .expect("a PNG");
                Ok(Screenshot::new(
                    base64::engine::general_purpose::STANDARD.encode(buf.into_inner()),
                    200,
                    200,
                ))
            }

            async fn read_ui_tree(
                &self,
                _app: Option<&str>,
            ) -> crate::computer::Result<Vec<crate::computer::UiElement>> {
                Ok(Vec::new())
            }

            async fn execute(
                &self,
                _action: DesktopAction,
            ) -> crate::computer::Result<ActionResult> {
                self.clicked.store(true, Ordering::SeqCst);
                Ok(ActionResult::success("clicked"))
            }

            async fn wait_for(
                &self,
                _condition: crate::computer::WaitCondition,
                _timeout: std::time::Duration,
            ) -> crate::computer::Result<bool> {
                Ok(true)
            }

            async fn display_geometry(
                &self,
            ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
                Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                    0, 0, 200, 200,
                )
                .primary()]))
            }
        }

        let loop_ = ComputerUseLoop::new(Arc::new(CornerPainter {
            clicked: AtomicBool::new(false),
        }))
        .with_config(LoopConfig {
            settle_delay_ms: 0,
            max_steps: 2,
            ..LoopConfig::default()
        });

        let result = loop_
            .run("click the thing", move |state| async move {
                if !state.history.is_empty() {
                    return Ok(LoopDecision::Done { message: "stop".to_string() });
                }
                Ok(LoopDecision::Action(DesktopAction::Click {
                    target: crate::computer::types::ClickTarget::Coordinate(
                        crate::computer::types::Point::new(10, 10),
                    ),
                    button: crate::computer::types::MouseButton::Left,
                }))
            })
            .await
            .expect("a run");

        assert_eq!(result.history.len(), 1, "one step ran");
        let record = &result.history[0];
        assert_eq!(record.verification, StepVerification::Unverifiable);
        let target = record.aim_missed.expect("the miss survives to the record");
        assert!(
            target.overlaps(&crate::computer::Rect::new(10, 10, 1, 1)),
            "the box is where the click aimed: {target:?}"
        );
        assert!(
            !target.overlaps(&crate::computer::Rect::new(150, 150, 32, 32)),
            "and not where the change was: {target:?}"
        );
    }

    /// A batch is one look and several actions.
    ///
    /// The cost this removes is the settle-and-capture after each action: on a
    /// form, `click → type → Tab → type` would otherwise be four looks. The
    /// assertion is on the adapter's capture count, because "the run
    /// finished and the history is right" passes with the batching removed
    /// entirely — the same actions would simply have cost four times as much.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_batch_looks_once_even_when_it_acts_several_times() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Captures count themselves; actions remember their order.
        struct CountingDesktop {
            captures: AtomicUsize,
            actions: std::sync::Mutex<Vec<String>>,
            /// Every capture and dispatch, in the order they happened.
            sequence: std::sync::Mutex<Vec<String>>,
        }

        #[async_trait::async_trait]
        impl crate::computer::ComputerAdapter for CountingDesktop {
            async fn screenshot(
                &self,
                _region: Option<crate::computer::Rect>,
            ) -> crate::computer::Result<Screenshot> {
                use base64::Engine as _;
                self.captures.fetch_add(1, Ordering::SeqCst);
                self.sequence.lock().unwrap().push("capture".to_string());
                let img = image::RgbImage::new(64, 64);
                let mut buf = std::io::Cursor::new(Vec::new());
                image::DynamicImage::ImageRgb8(img)
                    .write_to(&mut buf, image::ImageFormat::Png)
                    .expect("a PNG");
                Ok(Screenshot::new(
                    base64::engine::general_purpose::STANDARD.encode(buf.into_inner()),
                    64,
                    64,
                ))
            }

            async fn read_ui_tree(
                &self,
                _app: Option<&str>,
            ) -> crate::computer::Result<Vec<crate::computer::UiElement>> {
                Ok(Vec::new())
            }

            async fn execute(
                &self,
                action: DesktopAction,
            ) -> crate::computer::Result<ActionResult> {
                let name = crate::computer::driver::describe_action(&action);
                self.actions.lock().unwrap().push(name.clone());
                self.sequence.lock().unwrap().push(name);
                Ok(ActionResult::success("done"))
            }

            async fn wait_for(
                &self,
                _condition: crate::computer::WaitCondition,
                _timeout: std::time::Duration,
            ) -> crate::computer::Result<bool> {
                Ok(true)
            }

            async fn display_geometry(
                &self,
            ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
                Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                    0, 0, 64, 64,
                )
                .primary()]))
            }
        }

        let adapter = Arc::new(CountingDesktop {
            captures: AtomicUsize::new(0),
            actions: std::sync::Mutex::new(Vec::new()),
            sequence: std::sync::Mutex::new(Vec::new()),
        });
        let loop_ = ComputerUseLoop::new(adapter.clone()).with_config(LoopConfig {
            max_steps: 5,
            ..LoopConfig::default()
        });

        let asked = Arc::new(AtomicUsize::new(0));
        let asked_in_loop = asked.clone();
        let result = loop_
            .run("fill the form", move |state| {
                asked_in_loop.fetch_add(1, Ordering::SeqCst);
                let first = state.history.is_empty();
                async move {
                    if !first {
                        return Ok(LoopDecision::Done { message: "filled".to_string() });
                    }
                    Ok(LoopDecision::Batch(vec![
                        DesktopAction::Click {
                            target: crate::computer::types::ClickTarget::Coordinate(
                                crate::computer::types::Point::new(5, 5),
                            ),
                            button: crate::computer::types::MouseButton::Left,
                        },
                        DesktopAction::Type {
                            text: "Ada".to_string(),
                            target: None,
                        },
                        DesktopAction::KeyPress { keys: vec!["tab".to_string()] },
                        DesktopAction::Type {
                            text: "Lovelace".to_string(),
                            target: None,
                        },
                    ]))
                }
            })
            .await
            .expect("a run");

        assert_eq!(asked.load(Ordering::SeqCst), 2, "one look for the batch, one to finish");
        assert_eq!(result.history.len(), 4, "every action is its own step record");
        assert_eq!(
            adapter.actions.lock().unwrap().len(),
            4,
            "and every action was dispatched, in order"
        );

        // Where the captures happen is the point. The last action's
        // observation legitimately settles (that costs several polls), so a
        // raw count would pass or fail for reasons that have nothing to do
        // with batching. What must not happen is a capture *between* the
        // batch's actions: the first three dispatches have to be adjacent in
        // the sequence.
        let sequence = adapter.sequence.lock().unwrap().clone();
        let dispatches: Vec<usize> = sequence
            .iter()
            .enumerate()
            .filter(|(_, event)| event.as_str() != "capture")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(dispatches.len(), 4, "four actions were dispatched: {sequence:?}");
        for window in dispatches[..3].windows(2) {
            let between = &sequence[window[0] + 1..window[1]];
            assert!(
                between.is_empty(),
                "a batched action captured before the next one ran: {sequence:?}"
            );
        }
    }

    /// Only the batch's last action is observed, so only it can be verified
    /// against a diff. The earlier ones stay `Unverifiable` rather than
    /// claiming a reading nobody took.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_batch_observes_only_its_last_action() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct PaintsOnClick {
            clicked: AtomicBool,
        }

        #[async_trait::async_trait]
        impl crate::computer::ComputerAdapter for PaintsOnClick {
            async fn screenshot(
                &self,
                _region: Option<crate::computer::Rect>,
            ) -> crate::computer::Result<Screenshot> {
                use base64::Engine as _;
                let painted = self.clicked.load(Ordering::SeqCst);
                let mut img = image::RgbImage::new(64, 64);
                if painted {
                    // A full 32×32 block: the diff averages blocks and only
                    // counts one at three-channel distance > 30, so a
                    // half-painted block is a change the diff cannot see.
                    for y in 0..32 {
                        for x in 0..32 {
                            img.put_pixel(x, y, image::Rgb([255, 0, 0]));
                        }
                    }
                }
                let mut buf = std::io::Cursor::new(Vec::new());
                image::DynamicImage::ImageRgb8(img)
                    .write_to(&mut buf, image::ImageFormat::Png)
                    .expect("a PNG");
                Ok(Screenshot::new(
                    base64::engine::general_purpose::STANDARD.encode(buf.into_inner()),
                    64,
                    64,
                ))
            }

            async fn read_ui_tree(
                &self,
                _app: Option<&str>,
            ) -> crate::computer::Result<Vec<crate::computer::UiElement>> {
                Ok(Vec::new())
            }

            async fn execute(
                &self,
                action: DesktopAction,
            ) -> crate::computer::Result<ActionResult> {
                // Only the click paints: the type is dispatched first, and a
                // paint there would already be in the last action's baseline.
                if matches!(action, DesktopAction::Click { .. }) {
                    self.clicked.store(true, Ordering::SeqCst);
                }
                Ok(ActionResult::success("done"))
            }

            async fn wait_for(
                &self,
                _condition: crate::computer::WaitCondition,
                _timeout: std::time::Duration,
            ) -> crate::computer::Result<bool> {
                Ok(true)
            }

            async fn display_geometry(
                &self,
            ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
                Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                    0, 0, 64, 64,
                )
                .primary()]))
            }
        }

        let loop_ = ComputerUseLoop::new(Arc::new(PaintsOnClick {
            clicked: AtomicBool::new(false),
        }))
        .with_config(LoopConfig {
            max_steps: 4,
            ..LoopConfig::default()
        });

        let result = loop_
            .run("click and confirm", move |state| async move {
                if !state.history.is_empty() {
                    return Ok(LoopDecision::Done { message: "done".to_string() });
                }
                Ok(LoopDecision::Batch(vec![
                    DesktopAction::Type {
                        text: "ok".to_string(),
                        target: None,
                    },
                    DesktopAction::Click {
                        target: crate::computer::types::ClickTarget::Coordinate(
                            crate::computer::types::Point::new(4, 4),
                        ),
                        button: crate::computer::types::MouseButton::Left,
                    },
                ]))
            })
            .await
            .expect("a run");

        assert_eq!(result.history.len(), 2);
        assert_eq!(
            result.history[0].verification,
            StepVerification::Unverifiable,
            "the batched click was not looked at"
        );
        assert_eq!(
            result.history[1].verification,
            StepVerification::Verified,
            "the last action is observed, and the screen did change under it"
        );
    }

    /// The allowlist reaches the session the loop actually dispatches
    /// through — installing the handle on the builder is not enough if the
    /// rebuilt step engine kept the old one.
    #[test]
    fn a_loop_hands_its_allowlist_to_the_session_it_dispatches_through() {
        let adapter = Arc::new(crate::computer::headless::HeadlessComputerAdapter::new());
        let allow = crate::computer::app_scope::AppAllowlist::new(vec!["Safari".to_string()]);
        let loop_ = ComputerUseLoop::new(adapter)
            .with_allow_apps(allow.clone())
            // A later rebuild (a setting changed) must carry it forward.
            .with_auto_rollback_threshold(2);

        assert!(loop_
            .session
            .config()
            .allow_apps
            .refusal(Some("1Password"))
            .is_some());
        assert_eq!(loop_.session.config().allow_apps.refusal(Some("Safari")), None);
        // Same handle, so a config edit reaches a running loop.
        allow.replace(vec!["Finder".to_string()]);
        assert_eq!(loop_.session.config().allow_apps.refusal(Some("Finder")), None);
    }
}
