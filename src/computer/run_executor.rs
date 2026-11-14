//! Running one computer-use goal to completion, in the background.
//!
//! [`ComputerUseLoop`] is pure orchestration: it perceives, decides, acts and
//! verifies, and hands the deciding to a closure. This module supplies that
//! closure — the production one — and everything around it that a *gateway*
//! run needs and a library loop does not:
//!
//! - the per-run [`RunBudget`], enforced before each model call rather than
//!   reported after the money is gone;
//! - the live [`RunState`] that `computer.status` answers from;
//! - one [`RunEvent`] per completed step, so a client watches the run happen
//!   instead of polling for it;
//! - a structured handoff note when the run stops without finishing, routed
//!   through the approval queue so a person is actually asked.
//!
//! A note on where step reporting happens: the loop calls `decide` *before*
//! each action, so the decision closure is the only place that also sees
//! `LoopState::history` — meaning the step that just finished, with its result
//! and its verification. Reporting from there costs one step of lag and buys
//! the truth about what the last action did, which is exactly what a status
//! view needs to show. The tail is flushed when the loop returns, so a run
//! that ends on `Done` still reports its last step.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::computer::driver::{DecisionReport, DriverError, ModelDriver, StepContext};
use crate::computer::run_budget::{RunBudget, StopReason};
use crate::computer::run_registry::{finalize, ComputerRun, RunPhase};
use crate::computer::tree_budget::prune;
use crate::computer::types::{ActionEffect, Rung};
use crate::computer::use_loop::{
    ComputerUseLoop, LoopConfig, LoopDecision, LoopResult, LoopState, StepRecord, StepVerification,
};
use crate::computer::{ComputerAdapter, ComputerError, Screenshot};

/// How many unparseable answers a run tolerates before it asks for a person.
///
/// One retry is a model formatting its JSON differently; three in a row is a
/// model that cannot do this task, and continuing to pay for its attempts is
/// how a run burns a budget without moving.
pub const MAX_PARSE_FAILURES: usize = 2;

/// Something that happened during a run, for the gateway to broadcast.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    /// One step finished.
    Step {
        run_id: String,
        step: usize,
        /// What the action was, in words.
        action: String,
        /// Which rung of the ladder ran it, when the adapter said.
        rung: Option<Rung>,
        /// What the adapter established about the effect.
        effect: Option<ActionEffect>,
        /// CAS digest of the post-action screenshot, when one was stored.
        screenshot_ref: Option<String>,
        /// `None` when nothing could confirm or deny the action.
        verified: Option<bool>,
        model_calls: usize,
        images_attached: usize,
    },
    /// The run ended.
    Finished {
        run_id: String,
        phase: RunPhase,
        steps: usize,
        message: String,
        stop_reason: StopReason,
    },
}

/// The sink for run events.
///
/// A plain closure rather than a trait: there is exactly one implementation
/// (broadcast a `GatewayEvent`) and a closure keeps `computer` free of a
/// dependency on `gateway`.
pub type RunObserver = Arc<dyn Fn(RunEvent) + Send + Sync>;

/// Everything one run needs that the loop itself does not know about.
pub struct RunPlan {
    pub adapter: Arc<dyn ComputerAdapter>,
    pub driver: Arc<ModelDriver>,
    pub loop_config: LoopConfig,
    /// Whether the model behind `driver` accepts images.
    pub model_supports_vision: bool,
    /// The visual scan, when the configuration allows one.
    ///
    /// `None` means no scan is attempted at all — the perception plan then
    /// decides an image on its own, exactly as before B″. When it is `Some`,
    /// the resolver still decides per step whether anything can look.
    pub vision: Option<Arc<crate::computer::vision::look::VisionLook>>,
    /// The run's cost ceilings.
    pub budget: crate::config::RunBudgetConfig,
    pub observer: Option<RunObserver>,
}

/// The outcome of a run, as the caller reports it.
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub success: bool,
    pub message: String,
    pub steps: usize,
    pub model_calls: usize,
    pub images_attached: usize,
    pub stop_reason: StopReason,
    /// Present when the run stopped without meeting the goal.
    pub handoff: Option<crate::computer::handoff::HandoffNote>,
}

impl RunOutcome {
    /// A one-line description for a CLI or a log.
    pub fn describe(&self) -> String {
        format!(
            "{} ({} steps, {} model calls, {} images, {})",
            self.message,
            self.steps,
            self.model_calls,
            self.images_attached,
            self.stop_reason.describe()
        )
    }
}

/// Run one goal to completion, updating `run` as it goes.
///
/// Never returns an error for a *task* failure: a run that cannot finish is a
/// result with `success: false`, not an error. An `Err` here is the run
/// machinery itself being broken (no adapter, no provider), which the caller
/// reports as a failed request rather than as a failed goal.
pub async fn execute(run: Arc<ComputerRun>, plan: RunPlan) -> crate::Result<RunOutcome> {
    let budget = Arc::new(Mutex::new(RunBudget::new(plan.budget.clone())));
    let observer = plan.observer.clone();
    let outcome = match run_loop(run.clone(), plan, budget.clone()).await {
        Ok((outcome, spent)) => {
            let phase = if outcome.success {
                RunPhase::Done
            } else if outcome.stop_reason == StopReason::Cancelled {
                RunPhase::Cancelled
            } else {
                RunPhase::Failed
            };
            (outcome, spent, phase)
        }
        Err(e) => {
            // The machinery broke. Record it on the run so status is not left
            // saying "running" forever, then propagate.
            run.update(|s| {
                s.phase = RunPhase::Failed;
                s.message = Some(e.to_string());
                s.last_error = Some(e.to_string());
            })
            .await;
            return Err(e);
        }
    };

    let (outcome, spent, phase) = outcome;
    let success = outcome.success;
    let message = outcome.message.clone();
    let steps = outcome.steps;
    run.update(|s| {
        finalize(
            s,
            success,
            message.clone(),
            phase == RunPhase::Cancelled,
            outcome.stop_reason,
            steps,
            &spent,
        )
    })
    .await;
    // `finalize` sets the phase from success; a cancellation is neither
    // success nor a plain failure, so correct it afterwards.
    run.update(|s| s.phase = phase).await;

    if let Some(observer) = observer.as_ref() {
        observer(RunEvent::Finished {
            run_id: run.run_id.clone(),
            phase,
            steps: outcome.steps,
            message: outcome.message.clone(),
            stop_reason: outcome.stop_reason,
        });
    }

    Ok(outcome)
}

async fn run_loop(
    run: Arc<ComputerRun>,
    plan: RunPlan,
    budget: Arc<Mutex<RunBudget>>,
) -> crate::Result<(RunOutcome, RunBudget)> {
    let adapter = plan.adapter.clone();
    let driver = plan.driver.clone();
    let observer = plan.observer.clone();
    let model_supports_vision = plan.model_supports_vision;
    let vision = plan.vision.clone();
    let controller = run.controller.clone();
    let run_for_closure = run.clone();

    let computer_loop = ComputerUseLoop::new(plan.adapter.clone())
        .with_config(plan.loop_config)
        .with_execution_controller(controller.clone());

    // Shared with the closure: the parse-failure streak, and why the loop
    // stopped when the reason is ours rather than the loop's.
    let parse_failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let our_stop = Arc::new(Mutex::new(None::<StopReason>));

    let closure_failures = parse_failures.clone();
    let closure_stop = our_stop.clone();
    let closure_budget = budget.clone();
    let closure_run = run_for_closure.clone();
    let closure_vision = vision.clone();
    let closure_observer = observer.clone();
    let goal = run.goal.clone();

    let decided = computer_loop
        .run(&goal, move |state: LoopState| {
            let adapter = adapter.clone();
            let driver = driver.clone();
            let run = closure_run.clone();
            let budget = closure_budget.clone();
            let observer = closure_observer.clone();
            let parse_failures = closure_failures.clone();
            let our_stop = closure_stop.clone();
            let vision = closure_vision.clone();
            async move {
                // Announce the step that just finished (the first iteration
                // has none). Doing it here means the report carries the
                // action's real result and verification.
                announce_new_steps(&run, &state.history, observer.as_ref()).await;
                let done = state.history.len();
                run.update(|s| {
                    s.step = done;
                    s.last_verified = state.last_verified;
                    s.last_error = state.last_error.clone();
                    s.consecutive_failures = state.consecutive_failures;
                })
                .await;

                // Cost ceiling, checked *before* the call it would pay for.
                {
                    let mut b = budget.lock().await;
                    if let Err(exhausted) = b.record_call() {
                        drop(b);
                        *our_stop.lock().await = Some(StopReason::Budget);
                        return Ok(LoopDecision::NeedHelp {
                            reason: format!(
                                "stopped at step {}: {exhausted} — the goal is not finished",
                                state.step
                            ),
                        });
                    }
                }

                let ctx = observe(
                    &adapter,
                    &state,
                    &budget,
                    model_supports_vision,
                    vision.as_ref(),
                    &run,
                )
                .await;

                match driver.decide_reporting(&state, &ctx).await {
                    Ok(report) => {
                        parse_failures.store(0, std::sync::atomic::Ordering::Release);
                        record_decision(&budget, &run, &report).await;
                        // A control the scan found is not in the adapter's
                        // snapshot cache, so an id naming one resolves to
                        // nothing. Translate it to the point the scan
                        // established before the loop dispatches it.
                        Ok(rewrite_vision_target(report.decision, ctx.vision.as_ref()))
                    }
                    Err(e) => handle_driver_error(e, &parse_failures, &our_stop, &run).await,
                }
            }
        })
        .await?;

    // The loop is done; report any step it executed after the last decision.
    announce_new_steps(&run, &decided.history, observer.as_ref()).await;

    let runtime_state = controller.current_state().await;
    let stop_reason = {
        let ours = *our_stop.lock().await;
        ours.unwrap_or_else(|| classify(&decided, runtime_state))
    };
    let steps = decided.steps_taken.max(decided.history.len());
    let handoff = if decided.success {
        None
    } else {
        Some(crate::computer::handoff::note_for(
            &run,
            &decided.history,
            &decided.message,
            stop_reason,
        ))
    };

    let spent = {
        let b = budget.lock().await;
        b.clone()
    };

    Ok((
        RunOutcome {
            run_id: run.run_id.clone(),
            success: decided.success,
            message: decided.message,
            steps,
            model_calls: spent.model_calls(),
            images_attached: spent.images_attached(),
            stop_reason,
            handoff,
        },
        spent,
    ))
}

/// Read the screen for one step: the tree, whether it is whole, and a capture.
///
/// The tree is pruned by display geometry before it is handed on. An
/// off-screen window with a hundred children (OpenShot's detached emoji tool
/// window is the measured case) otherwise consumes the whole line budget and
/// hides the window the task is about — the model then decides against a
/// truncated, misleading screen.
async fn observe(
    adapter: &Arc<dyn ComputerAdapter>,
    state: &LoopState,
    budget: &Arc<Mutex<RunBudget>>,
    model_supports_vision: bool,
    vision: Option<&Arc<crate::computer::vision::look::VisionLook>>,
    run: &Arc<ComputerRun>,
) -> StepContext {
    let raw_tree = match adapter.read_ui_tree(None).await {
        Ok(tree) => tree,
        Err(e) => {
            debug!("read_ui_tree failed, deciding from the capture alone: {e}");
            Vec::new()
        }
    };

    // Completeness is judged on what was read, not on what survives pruning:
    // folding a window away is a decision this side made, while an incomplete
    // read is a fact about the screen that the model must be told.
    let tree_complete = crate::computer::types::UiElement::tree_is_complete(&raw_tree);
    let geometry = adapter.display_geometry().await.unwrap_or_default();
    let (tree, _report) = prune(&raw_tree, &geometry);

    let found = match vision {
        Some(look) => {
            look_for_controls(look, state, &raw_tree, tree_complete, &geometry, budget, run).await
        }
        None => None,
    };

    let image_budget_available = budget.lock().await.can_attach_image();
    StepContext {
        tree,
        tree_complete,
        screenshot: Some(state.screenshot.clone()),
        model_supports_vision,
        image_budget_available,
        vision: found,
    }
}

/// Run the visual scan for a step, if this step is one that needs eyes.
///
/// The trigger is the same one B′ uses to decide an image is warranted, minus
/// the image-specific gates: a truncated tree, an unverified last step, or
/// repeated failures. `model_supports_vision` is deliberately *not* part of it
/// — a local detector returns text, which a model without vision can use
/// perfectly well, and that is the whole point of putting vision at the top of
/// the ladder.
///
/// The outcome is written to the run either way. "It stopped and did not say
/// why it could not see" is the failure this recording prevents.
async fn look_for_controls(
    look: &crate::computer::vision::look::VisionLook,
    state: &LoopState,
    tree: &[crate::computer::types::UiElement],
    tree_complete: bool,
    geometry: &crate::computer::display::DisplayGeometry,
    budget: &Arc<Mutex<RunBudget>>,
    run: &Arc<ComputerRun>,
) -> Option<crate::computer::vision::look::VisionElements> {
    let wanted =
        !tree_complete || state.last_verified == Some(false) || state.consecutive_failures >= 2;
    if !wanted {
        return None;
    }
    // Nothing to look at: a capture the loop could not take is not a question
    // for a vision model.
    if state.screenshot.base64.is_empty() {
        return None;
    }

    let outcome = look
        .look(&state.screenshot, &state.goal, geometry, budget)
        .await;
    let status = outcome.status();
    debug!("vision step {}: {status}", state.step);
    run.update(|s| s.vision = Some(status)).await;

    // Even an empty scan is carried through: `compose` treats "the scan ran
    // and found nothing" as "the image rung still applies", which is why it
    // checks `is_empty` rather than `is_none`.
    match outcome {
        crate::computer::vision::look::LookOutcome::Found(found) => Some(found),
        _ => {
            let _ = tree;
            None
        }
    }
}

/// Rewrite a target that names a visually-detected control into a point.
///
/// The adapter resolves accessibility ids against its own snapshot cache, which
/// never held these elements — handing it one would answer "stale or unknown",
/// and the ladder would report the click as failed when nothing was ever
/// attempted. The scan did establish where the control is, so the honest
/// translation is a coordinate. Targets that are not ours pass through
/// untouched, which is what keeps an accessibility id working exactly as
/// before.
fn retarget(
    target: crate::computer::types::ClickTarget,
    vision: Option<&crate::computer::vision::look::VisionElements>,
) -> crate::computer::types::ClickTarget {
    use crate::computer::types::ClickTarget;

    let ClickTarget::ElementId(id) = &target else {
        return target;
    };
    let Some(vision) = vision else {
        return target;
    };
    match vision.resolve(id) {
        Some(point) => ClickTarget::Coordinate(point),
        None => target,
    }
}

/// Apply [`retarget`] to wherever a decision carries a target.
///
/// Only `Action` carries one; `Done` and `NeedHelp` are passed through so the
/// call site does not have to match twice.
fn rewrite_vision_target(
    decision: LoopDecision,
    vision: Option<&crate::computer::vision::look::VisionElements>,
) -> LoopDecision {
    match decision {
        LoopDecision::Action(action) => LoopDecision::Action(retarget_action(action, vision)),
        other => other,
    }
}

/// Apply [`retarget`] to wherever an action carries a target.
fn retarget_action(
    action: crate::computer::types::DesktopAction,
    vision: Option<&crate::computer::vision::look::VisionElements>,
) -> crate::computer::types::DesktopAction {
    use crate::computer::types::DesktopAction;

    match action {
        DesktopAction::Click { target, button } => DesktopAction::Click {
            target: retarget(target, vision),
            button,
        },
        DesktopAction::DoubleClick { target, button } => DesktopAction::DoubleClick {
            target: retarget(target, vision),
            button,
        },
        DesktopAction::Type { text, target } => DesktopAction::Type {
            text,
            target: target.map(|t| retarget(t, vision)),
        },
        DesktopAction::Scroll { target, direction, amount } => DesktopAction::Scroll {
            target: retarget(target, vision),
            direction,
            amount,
        },
        DesktopAction::Drag { from, to } => DesktopAction::Drag {
            from: retarget(from, vision),
            to: retarget(to, vision),
        },
        other => other,
    }
}

/// A driver failure, turned into something the loop can act on.
///
/// Unparseable answers are retried — the loop is re-asked rather than stopped,
/// and the retry is a pure observation so nothing is done on a garbled
/// instruction. Anything else (the provider is down, no provider at all) ends
/// the run with a person-facing message, because re-asking a broken provider
/// spends the budget to learn the same thing.
async fn handle_driver_error(
    error: DriverError,
    parse_failures: &Arc<std::sync::atomic::AtomicUsize>,
    our_stop: &Arc<Mutex<Option<StopReason>>>,
    run: &Arc<ComputerRun>,
) -> Result<LoopDecision, ComputerError> {
    match error {
        DriverError::Unparseable(why) => {
            let failures = parse_failures.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            run.update(|s| s.last_error = Some(format!("unreadable answer: {why}")))
                .await;
            if failures > MAX_PARSE_FAILURES {
                *our_stop.lock().await = Some(StopReason::NeedHelp);
                Ok(LoopDecision::NeedHelp {
                    reason: format!(
                        "the decision model kept answering with something that is not a \
                         decision ({why}); nothing was done on any of them"
                    ),
                })
            } else {
                // Re-observe. An unreadable answer establishes nothing, so the
                // next step reads the screen again instead of acting on a
                // guess — this is the "parse failure is Unverifiable, not
                // Done" rule applied where it can still help.
                Ok(LoopDecision::Action(crate::computer::types::DesktopAction::ReadUiTree {
                    app: None,
                }))
            }
        }
        other => {
            *our_stop.lock().await = Some(StopReason::NeedHelp);
            Ok(LoopDecision::NeedHelp { reason: other.to_string() })
        }
    }
}

async fn record_decision(
    budget: &Arc<Mutex<RunBudget>>,
    run: &Arc<ComputerRun>,
    report: &DecisionReport,
) {
    let mut b = budget.lock().await;
    if report.image_attached {
        // `record_images` cannot fail here: the driver asked for an image only
        // after `can_attach_image` said yes. If it somehow does, the run keeps
        // going on text — the ceiling was reported at the point of refusal.
        if let Err(e) = b.record_images(1) {
            debug!("image budget race: {e}");
        }
    }
    b.record_tokens(report.tokens_in, report.tokens_out);
    let calls = b.model_calls();
    let images = b.images_attached();
    drop(b);
    run.update(|s| {
        s.model_calls = calls;
        s.images_attached = images;
    })
    .await;
}

/// Report every executed step the run has not reported yet.
async fn announce_new_steps(
    run: &Arc<ComputerRun>,
    history: &[StepRecord],
    observer: Option<&RunObserver>,
) {
    let already = run.snapshot().await.completed.len();
    if history.len() <= already {
        return;
    }

    for record in &history[already..] {
        let screenshot_ref = store_screenshot(record.screenshot_after.as_ref()).await;
        let rung = record.result.evidence.as_ref().map(|e| e.rung);
        let effect = record.result.evidence.as_ref().map(|e| e.effect);
        let verified = match record.verification {
            StepVerification::Verified => Some(true),
            StepVerification::Failed => Some(false),
            StepVerification::Unverifiable => None,
        };
        let action = crate::computer::driver::describe_action(&record.action);
        let line = describe_step(record);

        let (calls, images) = {
            let state = run.snapshot().await;
            (state.model_calls, state.images_attached)
        };
        run.update(|s| s.completed.push(line.clone())).await;

        if let Some(observer) = observer {
            observer(RunEvent::Step {
                run_id: run.run_id.clone(),
                step: record.step,
                action,
                rung,
                effect,
                screenshot_ref,
                verified,
                model_calls: calls,
                images_attached: images,
            });
        }
    }
}

/// One line per step for the handoff note.
fn describe_step(record: &StepRecord) -> String {
    let verdict = match record.verification {
        StepVerification::Verified => "ok",
        StepVerification::Failed => "failed",
        StepVerification::Unverifiable => "unchecked",
    };
    format!(
        "{}. {} → {} ({verdict})",
        record.step + 1,
        crate::computer::driver::describe_action(&record.action),
        truncate(&record.result.message, 100)
    )
}

fn truncate(text: &str, max: usize) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let mut chars = flat.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Store a post-action capture in the CAS and return its digest.
///
/// A digest rather than bytes: the step event is broadcast to every client,
/// and a base64 screenshot per step would make the event stream larger than
/// the run. Failure is logged and reported as `None` — a missing preview must
/// not fail a step that otherwise worked.
async fn store_screenshot(shot: Option<&Screenshot>) -> Option<String> {
    let shot = shot?;
    let bytes = if let Some(path) = &shot.file_path {
        match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!("could not read screenshot {}: {e}", path.display());
                return None;
            }
        }
    } else if !shot.base64.is_empty() {
        use base64::Engine as _;
        match base64::engine::general_purpose::STANDARD.decode(&shot.base64) {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!("could not decode screenshot: {e}");
                return None;
            }
        }
    } else {
        return None;
    };

    match tokio::task::spawn_blocking(move || crate::attachments::store_bytes(&bytes, "image/png"))
        .await
    {
        Ok(Ok(aref)) => Some(aref.digest),
        Ok(Err(e)) => {
            warn!("could not store step screenshot: {e}");
            None
        }
        Err(e) => {
            warn!("screenshot store task failed: {e}");
            None
        }
    }
}

/// Why the loop stopped, when it did not say so itself.
fn classify(result: &LoopResult, runtime: crate::acp::config::RuntimeState) -> StopReason {
    if result.success {
        return StopReason::Done;
    }
    let message = result.message.to_ascii_lowercase();
    if runtime == crate::acp::config::RuntimeState::Cancelled || message.contains("cancelled") {
        return StopReason::Cancelled;
    }
    if message.starts_with("max steps") {
        return StopReason::MaxSteps;
    }
    StopReason::NeedHelp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::config::RuntimeState;

    #[test]
    fn a_success_is_done() {
        let result = LoopResult {
            success: true,
            steps_taken: 3,
            history: vec![],
            final_screenshot: Screenshot::new(String::new(), 0, 0),
            message: "all good".to_string(),
        };
        assert_eq!(classify(&result, RuntimeState::Idle), StopReason::Done);
    }

    /// The loop reports "max steps reached" while still making progress; that
    /// is a ceiling, not a dead end, and the distinction drives whether a
    /// caller retries with more room.
    #[test]
    fn the_step_ceiling_is_its_own_reason() {
        let result = LoopResult {
            success: false,
            steps_taken: 30,
            history: vec![],
            final_screenshot: Screenshot::new(String::new(), 0, 0),
            message: "Max steps (30) reached".to_string(),
        };
        let reason = classify(&result, RuntimeState::Idle);
        assert_eq!(reason, StopReason::MaxSteps);
        assert!(reason.is_a_ceiling());
    }

    #[test]
    fn a_controller_cancel_is_a_cancellation() {
        let result = LoopResult {
            success: false,
            steps_taken: 2,
            history: vec![],
            final_screenshot: Screenshot::new(String::new(), 0, 0),
            message: "Execution cancelled by user".to_string(),
        };
        assert_eq!(classify(&result, RuntimeState::Idle), StopReason::Cancelled);
        // And by runtime state alone, with a message that says nothing.
        let quiet = LoopResult {
            message: "stopped".to_string(),
            ..result
        };
        assert_eq!(classify(&quiet, RuntimeState::Cancelled), StopReason::Cancelled);
    }

    #[test]
    fn a_stuck_run_asks_for_a_person() {
        let result = LoopResult {
            success: false,
            steps_taken: 6,
            history: vec![],
            final_screenshot: Screenshot::new(String::new(), 0, 0),
            message: "Stuck after 5 consecutive failures".to_string(),
        };
        assert_eq!(classify(&result, RuntimeState::Idle), StopReason::NeedHelp);
    }

    /// A ceiling reason must never be confused with a completion: the whole
    /// point of the enum is that "ran out of room" and "finished" are
    /// different answers.
    #[test]
    fn a_ceiling_is_never_a_success() {
        assert!(StopReason::MaxSteps.is_a_ceiling());
        assert_ne!(StopReason::MaxSteps, StopReason::Done);
        assert_ne!(StopReason::Budget, StopReason::Done);
        assert!(!StopReason::Done.is_a_ceiling());
    }

    #[test]
    fn truncate_keeps_the_line_short() {
        assert_eq!(truncate("hello", 100), "hello");
        assert_eq!(truncate("a\nb", 100), "a b");
        let long = "x".repeat(200);
        let cut = truncate(&long, 10);
        assert_eq!(cut.chars().count(), 11);
        assert!(cut.ends_with('…'));
    }

    // ── a real run, end to end ──────────────────────────────────────────
    //
    // The point of these is B′'s central claim: a step costs one model call,
    // and an image only when the text cannot carry the question. Counting
    // requests is the only way to check it — the loop's own report says what
    // perception *decided*, and the bill is what the provider was actually
    // sent.

    use crate::computer::{ActionResult, Rect, WaitCondition};
    use crate::providers::{CompletionRequest, CompletionResponse, CompletionStream, Message};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    /// A one-pixel PNG: `can_attach` refuses an empty payload, so a
    /// placeholder string would make every image assertion vacuous.
    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    /// Answers with a fixed list, then repeats the last answer forever.
    ///
    /// Repeating rather than erroring matters: a run that ends with `Done`
    /// never asks again, so an over-long script would hide a bug where the
    /// loop calls one more time than it should.
    struct ScriptedProvider {
        answers: Vec<String>,
        calls: AtomicUsize,
        requests: StdMutex<Vec<CompletionRequest>>,
        /// Answer the first question by clicking whatever element id the
        /// prompt shows.
        ///
        /// Used by the vision test: a scan's id is minted per process, so the
        /// only way to click the control the scan found is to read the id out
        /// of the prompt — which is also the strongest form of the assertion,
        /// because it proves the prompt really carried it.
        click_first_element_id: std::sync::atomic::AtomicBool,
    }

    impl ScriptedProvider {
        fn new(answers: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                answers: answers.into_iter().map(str::to_string).collect(),
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
                click_first_element_id: std::sync::atomic::AtomicBool::new(false),
            })
        }

        /// Click the first element id in the prompt on the first call.
        fn clicking_the_first_element(self: &Arc<Self>) -> Arc<Self> {
            self.click_first_element_id.store(true, Ordering::SeqCst);
            self.clone()
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn requests(&self) -> Vec<CompletionRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn images_sent(&self) -> usize {
            self.requests()
                .iter()
                .filter(|r| {
                    r.messages
                        .iter()
                        .any(|m| m.role == crate::providers::Role::User && m.has_images())
                })
                .count()
        }
    }

    #[async_trait]
    impl crate::providers::Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn default_model(&self) -> &str {
            "test-model"
        }

        fn supports_tools(&self) -> bool {
            false
        }

        fn max_context(&self) -> usize {
            8_192
        }

        async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let answer = if index == 0 && self.click_first_element_id.load(Ordering::SeqCst) {
                let id = first_element_id(&request);
                format!(
                    r#"{{"decision": "act", "action": {{"type": "click", "element_id": "{id}"}}}}"#
                )
            } else {
                self.answers
                    .get(index)
                    .or_else(|| self.answers.last())
                    .cloned()
                    .unwrap_or_default()
            };
            self.requests.lock().unwrap().push(request);
            Ok(CompletionResponse {
                message: Message::assistant(answer),
                usage: None,
                model: "test-model".to_string(),
                finish_reason: None,
            })
        }

        async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
            Ok(Box::pin(tokio_stream::iter(vec![])))
        }

        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }

        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    /// The first `[id=...]` in the prompt's user message.
    fn first_element_id(request: &CompletionRequest) -> String {
        let text = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Only the detected section: the accessibility outline above it is
        // full of ids too, and one of those would click the wrong thing — it
        // resolves, so the mistake would go unnoticed.
        let text = text
            .split_once("Detected from the screenshot:")
            .expect("the scan's section is in the prompt")
            .1;
        let start = text.find("[id=").expect("the prompt shows an element id") + 4;
        let rest = &text[start..];
        rest[..rest.find(']').expect("the id is closed")].to_string()
    }

    /// A desktop that reports a small, whole tree and settles instantly.
    ///
    /// `tree_incomplete` flips the one fact that makes a step want an image,
    /// which is how the image-budget test drives perception without a real
    /// screen.
    struct FakeDesktop {
        tree_incomplete: bool,
        executions: AtomicUsize,
        /// What the loop actually dispatched, in order.
        actions: StdMutex<Vec<crate::computer::DesktopAction>>,
    }

    impl FakeDesktop {
        fn new(tree_incomplete: bool) -> Arc<Self> {
            Arc::new(Self {
                tree_incomplete,
                executions: AtomicUsize::new(0),
                actions: StdMutex::new(Vec::new()),
            })
        }

        fn executions(&self) -> usize {
            self.executions.load(Ordering::SeqCst)
        }

        fn actions(&self) -> Vec<crate::computer::DesktopAction> {
            self.actions.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ComputerAdapter for FakeDesktop {
        async fn screenshot(&self, _region: Option<Rect>) -> crate::computer::Result<Screenshot> {
            // The declared size matches the display, so image pixels and
            // screen points coincide and a test can assert a coordinate
            // without also asserting the scale factor.
            Ok(Screenshot::new(ONE_PIXEL_PNG.to_string(), 100, 100))
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<crate::computer::UiElement>> {
            Ok(vec![crate::computer::UiElement {
                id: "w1".to_string(),
                role: "window".to_string(),
                label: Some("Calculator".to_string()),
                value: None,
                bounds: Some(Rect::new(0, 0, 100, 100)),
                enabled: crate::computer::Tri::Known(true),
                focused: crate::computer::Tri::Known(true),
                actions: None,
                children: Vec::new(),
                children_count: None,
                truncated: self.tree_incomplete,
            }])
        }

        async fn execute(
            &self,
            action: crate::computer::DesktopAction,
        ) -> crate::computer::Result<ActionResult> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.actions.lock().unwrap().push(action);
            Ok(ActionResult::success("done"))
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: std::time::Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }

        async fn display_geometry(
            &self,
        ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
            Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 100, 100,
            )
            .primary()]))
        }

        async fn resolve_target(
            &self,
            target: &crate::computer::ClickTarget,
        ) -> crate::computer::Result<crate::computer::ActionTarget> {
            match target {
                crate::computer::ClickTarget::Coordinate(p) => {
                    Ok(crate::computer::ActionTarget::point(p.x, p.y))
                }
                // A fake desktop resolves every semantic target to the one
                // control it pretends to have.
                _ => Ok(crate::computer::ActionTarget::point(10, 10)),
            }
        }
    }

    /// A plan whose settling does no waiting, so a run test is not a sleep.
    fn fast_plan(
        provider: Arc<ScriptedProvider>,
        adapter: Arc<FakeDesktop>,
        images: usize,
        model_supports_vision: bool,
    ) -> RunPlan {
        let driver = ModelDriver::new(provider, "test-model");
        RunPlan {
            adapter,
            driver: Arc::new(driver),
            loop_config: LoopConfig {
                max_steps: 10,
                settle_delay_ms: 0,
                verify_after_each: false,
                ..Default::default()
            },
            model_supports_vision,
            vision: None,
            budget: crate::config::RunBudgetConfig {
                max_calls_per_run: 20,
                max_images_per_run: images,
            },
            observer: None,
        }
    }

    fn click_answer() -> &'static str {
        r#"{"decision": "act", "action": {"type": "click", "x": 10, "y": 10}}"#
    }

    /// Five actions, then done: five model calls and no images.
    ///
    /// A complete tree with no failures is the ordinary case, and it must be
    /// the cheap one — one call per step, text only.
    #[tokio::test]
    async fn a_five_step_run_makes_exactly_five_model_calls() {
        let provider = ScriptedProvider::new(vec![
            click_answer(),
            click_answer(),
            click_answer(),
            click_answer(),
            click_answer(),
            r#"{"decision": "done", "message": "opened it"}"#,
        ]);
        let adapter = FakeDesktop::new(false);
        let run = ComputerRun::new("r1", "open the calculator");

        let outcome = execute(run.clone(), fast_plan(provider.clone(), adapter.clone(), 15, true))
            .await
            .expect("the run machinery works");

        assert!(outcome.success, "{}", outcome.message);
        assert_eq!(outcome.steps, 5);
        assert_eq!(provider.calls(), 6, "five actions plus the finishing decision");
        assert_eq!(outcome.model_calls, 6);
        assert_eq!(adapter.executions(), 5);
        assert_eq!(provider.images_sent(), 0, "a complete tree with no failures needs no image");
        assert_eq!(outcome.images_attached, 0);
        assert_eq!(outcome.stop_reason, StopReason::Done);
        assert!(outcome.handoff.is_none(), "a finished run needs no handover");
    }

    /// An incomplete tree asks for an image, and the image ceiling stops it
    /// after `max_images_per_run` — the run continues on text rather than
    /// stopping, because stopping mid-goal over an image is worse than the
    /// tokens.
    #[tokio::test]
    async fn the_image_ceiling_caps_images_without_ending_the_run() {
        let provider = ScriptedProvider::new(vec![
            click_answer(),
            click_answer(),
            click_answer(),
            click_answer(),
            r#"{"decision": "done", "message": "opened it"}"#,
        ]);
        let adapter = FakeDesktop::new(true);
        let run = ComputerRun::new("r1", "open the calculator");

        let outcome = execute(run, fast_plan(provider.clone(), adapter, 2, true))
            .await
            .expect("the run machinery works");

        assert!(outcome.success);
        assert_eq!(
            outcome.images_attached, 2,
            "the ceiling is a ceiling: 4 steps wanted an image, 2 were allowed"
        );
        assert_eq!(provider.images_sent(), 2, "and exactly those two carried one");
        assert!(provider.calls() > 2, "the run kept going on text");
    }

    /// The same image ceiling with a model that cannot see: not one image is
    /// attached, however incomplete the tree is.
    #[tokio::test]
    async fn a_model_without_vision_is_never_sent_an_image_in_a_run() {
        let provider = ScriptedProvider::new(vec![
            click_answer(),
            r#"{"decision": "done", "message": "opened it"}"#,
        ]);
        let adapter = FakeDesktop::new(true);
        let run = ComputerRun::new("r1", "open the calculator");

        let outcome = execute(run, fast_plan(provider.clone(), adapter, 15, false))
            .await
            .expect("the run machinery works");

        assert!(outcome.success);
        assert_eq!(provider.images_sent(), 0);
        assert_eq!(outcome.images_attached, 0);
    }

    /// A run that hits the call ceiling stops with `Budget` and keeps the
    /// steps it completed — it does not report a failure of the goal.
    #[tokio::test]
    async fn the_call_ceiling_stops_the_run_and_keeps_its_progress() {
        let provider = ScriptedProvider::new(vec![click_answer()]);
        let adapter = FakeDesktop::new(false);
        let run = ComputerRun::new("r1", "open the calculator");

        let mut plan = fast_plan(provider.clone(), adapter, 15, true);
        plan.budget.max_calls_per_run = 2;

        let outcome = execute(run.clone(), plan)
            .await
            .expect("the run machinery works");

        assert!(!outcome.success);
        assert_eq!(outcome.stop_reason, StopReason::Budget);
        assert_eq!(provider.calls(), 2, "the ceiling is checked before the call");
        assert_eq!(outcome.model_calls, 2);
        assert!(outcome.steps >= 1, "the completed steps are kept: {}", outcome.steps);
        let handoff = outcome.handoff.expect("a stopped run hands over");
        assert_eq!(handoff.stop_reason, StopReason::Budget);
        assert!(
            handoff.next.contains("budget") || handoff.next.contains("computer.budget"),
            "the advice names the knob: {}",
            handoff.next
        );

        // …and the run's live state says the same thing.
        let view = run.view().await;
        assert_eq!(view["phase"], "failed");
        assert_eq!(view["finished"], true);
        assert_eq!(view["summary"]["stop_reason"], "budget");
    }

    /// An unreadable answer is retried, never acted on. The retry is a pure
    /// observation, so a garbled instruction cannot click anything.
    #[tokio::test]
    async fn an_unreadable_answer_is_retried_as_an_observation() {
        let provider = ScriptedProvider::new(vec![
            "I think you should click the button",
            r#"{"decision": "done", "message": "fine"}"#,
        ]);
        let adapter = FakeDesktop::new(false);
        let run = ComputerRun::new("r1", "open the calculator");

        let outcome = execute(run, fast_plan(provider.clone(), adapter.clone(), 15, true))
            .await
            .expect("the run machinery works");

        assert!(outcome.success);
        assert_eq!(provider.calls(), 2, "the garbled answer was re-asked");
        assert_eq!(
            adapter.executions(),
            1,
            "only the `read_ui_tree` observation ran — no click was dispatched"
        );
    }

    /// A repeated identical screenshot must not be stored twice: the CAS is
    /// content-addressed, so the second step's digest is the first one's.
    #[tokio::test]
    async fn an_identical_screenshot_is_stored_once() {
        let shot = Screenshot::new(ONE_PIXEL_PNG.to_string(), 1, 1);
        let first = store_screenshot(Some(&shot)).await;
        let second = store_screenshot(Some(&shot)).await;
        assert!(first.is_some(), "a one-pixel PNG stores fine");
        assert_eq!(first, second, "the digest is the content, so it repeats");
    }

    // ── vision, against a real loop ────────────────────────────────────

    /// A scan's element ids are rewritten to points before the loop acts.
    ///
    /// This is the join between the two halves of the vision path, and it has
    /// to be checked on a real run: the driver renders detected controls, the
    /// model answers with their id, and if nothing translated that id the
    /// adapter would answer "stale or unknown" and the run would report a
    /// failure for a click that was never attempted.
    #[tokio::test]
    async fn a_click_on_a_detected_control_is_dispatched_at_its_point() {
        // A truncated tree is what makes step 0 scan at all; the model then
        // answers by clicking the id read out of the prompt, which is the only
        // way to prove the id really travelled that far.
        let provider =
            ScriptedProvider::new(vec!["", r#"{"decision": "done", "message": "clicked it"}"#])
                .clicking_the_first_element();
        let adapter = FakeDesktop::new(true);
        let run = ComputerRun::new("r1", "click Save");

        let mut plan = fast_plan(provider.clone(), adapter.clone(), 0, true);
        plan.vision = Some(fixed_look());

        let outcome = execute(run.clone(), plan)
            .await
            .expect("the run machinery works");

        assert!(outcome.success, "{}", outcome.message);
        // The click reached the adapter, and it reached it as a point: an id
        // would have been answered by `resolve_target` with "unknown", which
        // the ladder reports as a failed click.
        let actions = adapter.actions();
        match actions.first() {
            Some(crate::computer::DesktopAction::Click {
                target: crate::computer::ClickTarget::Coordinate(point),
                ..
            }) => {
                // "Save" is at (10,20) 30x10 of a 100x100 image on a 100x100
                // display, so its centre is (25,25).
                assert_eq!((point.x, point.y), (25, 25));
            }
            other => panic!("the click was not dispatched at the scan's point: {other:?}"),
        }
        // And the run recorded what the scan established.
        let view = run.view().await;
        assert_eq!(view["vision"], "api: 1 controls");
    }

    /// No vision anywhere ends as `need_help` with the reason stated.
    ///
    /// The plan's last verification item: a run that cannot see must say so,
    /// not spin. The model is given no usable answer here on purpose — the
    /// point is what the *run* reports.
    #[tokio::test]
    async fn a_blind_run_ends_asking_for_a_person_and_says_why() {
        let provider = ScriptedProvider::new(vec![
            r#"{"decision": "help", "reason": "the accessibility tree has no Save button and I cannot see the screen"}"#,
        ]);
        let adapter = FakeDesktop::new(true);
        let run = ComputerRun::new("r1", "click Save");

        let mut plan = fast_plan(provider, adapter, 0, false);
        plan.vision = Some(blind_look());

        let outcome = execute(run.clone(), plan)
            .await
            .expect("the run machinery works");

        assert!(!outcome.success);
        assert_eq!(outcome.stop_reason, StopReason::NeedHelp);
        let handoff = outcome.handoff.expect("a blind run hands over");
        assert_eq!(handoff.stop_reason, StopReason::NeedHelp);

        // The run log names the missing capability rather than leaving a
        // reader to guess why nothing happened.
        let view = run.view().await;
        let vision = view["vision"].as_str().expect("the reason was recorded");
        assert!(vision.contains("unavailable"), "the run says nobody could look: {vision}");
        // `off` mode's explanation names the setting.
        assert!(
            outcome.message.contains("Save")
                || view["message"].as_str().is_some_and(|m| m.contains("Save")),
            "the reason survives: {:?}",
            view["message"]
        );
    }

    /// A scan that found controls stands in for the image, so a run with a
    /// scan sends no images even when the tree is truncated.
    #[tokio::test]
    async fn a_run_with_a_scan_sends_no_images() {
        let provider = ScriptedProvider::new(vec![
            click_answer(),
            r#"{"decision": "done", "message": "clicked it"}"#,
        ]);
        let adapter = FakeDesktop::new(true);
        let run = ComputerRun::new("r1", "click Save");

        let mut plan = fast_plan(provider.clone(), adapter, 15, true);
        plan.vision = Some(fixed_look());

        let outcome = execute(run, plan).await.expect("the run machinery works");

        assert!(outcome.success, "{}", outcome.message);
        assert_eq!(outcome.images_attached, 0);
        assert_eq!(provider.images_sent(), 0);
    }

    /// A scan whose resolver cannot see leaves the image rung alone: the
    /// model still gets the picture it asked for.
    #[tokio::test]
    async fn a_blind_resolver_leaves_the_image_path_intact() {
        // A truncated tree wants a picture; nobody can scan, so the picture is
        // still the answer. This is the counterpart to
        // `a_run_with_a_scan_sends_no_images`: an empty scan must not be
        // mistaken for a scan that answered.
        let provider = ScriptedProvider::new(vec![
            r#"{"decision": "done", "message": "read it off the image"}"#,
        ]);
        let adapter = FakeDesktop::new(true);
        let run = ComputerRun::new("r1", "click Save");

        let mut plan = fast_plan(provider.clone(), adapter, 15, true);
        plan.vision = Some(blind_look());

        let outcome = execute(run, plan).await.expect("the run machinery works");

        assert!(outcome.success, "{}", outcome.message);
        assert_eq!(
            provider.images_sent(),
            1,
            "a scan that saw nothing must not take the image away"
        );
        assert_eq!(outcome.images_attached, 1);
    }

    /// A scan that always answers with one control, through the API rung so
    /// the answer is deterministic and costs no network.
    fn fixed_look() -> Arc<crate::computer::vision::look::VisionLook> {
        crate::computer::vision::look::test_support::api_look(
            r#"{"elements": [{"role": "button", "label": "Save", "x": 10, "y": 20, "width": 30, "height": 10}]}"#,
        )
    }

    /// A scan whose resolver can see nothing at all.
    fn blind_look() -> Arc<crate::computer::vision::look::VisionLook> {
        crate::computer::vision::look::test_support::off_look()
    }
}
