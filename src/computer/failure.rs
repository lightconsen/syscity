//! A run that stopped short, as a badcase.
//!
//! Stage D′ turns a failure into evidence: when a run ends `need_help`, `budget`
//! or `max_steps`, the gateway writes one row into the pending badcase pool
//! carrying what a person needs to reproduce and triage it — the goal, the step
//! that failed, the last capture, the action-ladder distribution, and the
//! handoff note the person was shown.
//!
//! This module is only the *conversion*: run types in, a plain
//! [`crate::eval::RunFailureRecord`] out. Nothing here touches the database or
//! the desktop, so what a failure contains can be read and asserted without a
//! gateway. The producer that calls it lives in the gateway, where the store and
//! the finished run are both in hand.
//!
//! Two boundaries are deliberate:
//!
//! - **Only failures.** A `done` run is not evidence of a bug and a `cancelled`
//!   one is a person's decision; neither is queued, and neither has a
//!   checkpoint left to read. [`failure_of`] therefore returns `None` for them
//!   rather than an empty record.
//! - **No model opinion.** The record says what the run *did* (verification
//!   verdicts, rung evidence, the adapter's result lines) and never infers that
//!   the goal was met. An expected end state is a person's job — see
//!   `ExpectedEndState` in the eval layer.

use crate::computer::handoff::HandoffNote;
use crate::computer::run_budget::StopReason;
use crate::computer::run_store::Checkpoint;
use crate::computer::use_loop::StepVerification;
use crate::eval::{RunFailureRecord, RunStepRecord};

/// The badcase for a run that stopped short, or `None` when it did not.
pub fn failure_of(
    checkpoint: &Checkpoint,
    handoff: Option<&HandoffNote>,
) -> Option<RunFailureRecord> {
    let stop_reason = checkpoint.stop_reason?;
    if !is_a_failure(stop_reason) {
        return None;
    }

    let failed_step = checkpoint.first_failed_step().map(|step| RunStepRecord {
        step: step.step,
        action: crate::computer::driver::describe_action(&step.action),
        result: step.result.clone(),
        verification: verification_word(step.verification).to_string(),
        rung: step.rung.map(|r| r.rung_name().to_string()),
    });

    // The run's last capture, whichever step took it. `first_failed_step` is
    // the obstacle, but the *last* frame is what the screen looked like when
    // the run gave up, which is what a person re-opening the desktop wants.
    let last_screenshot_ref = checkpoint
        .completed
        .iter()
        .rev()
        .find_map(|s| s.screenshot_ref.clone());

    Some(RunFailureRecord {
        run_id: checkpoint.run_id.clone(),
        goal: checkpoint.goal.clone(),
        stop_reason: stop_reason_word(stop_reason).to_string(),
        steps_completed: checkpoint.completed.len(),
        model_calls: checkpoint.model_calls,
        images_attached: checkpoint.images_attached,
        failed_step,
        last_screenshot_ref,
        rung_distribution: checkpoint
            .rung_distribution()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        handoff: handoff.and_then(|note| serde_json::to_value(note).ok()),
        step_lines: checkpoint
            .completed
            .iter()
            .map(|s| {
                crate::computer::run_store::step_line(
                    s.step,
                    &crate::computer::driver::describe_action(&s.action),
                    &s.result,
                    crate::computer::run_store::verdict_word(s.verification),
                )
            })
            .collect(),
    })
}

/// Whether a reason is a failure worth collecting.
///
/// The two ceilings count: a run that ran out of room with the goal unmet is a
/// signal that the ladder, the prompt or the budget is wrong, which is exactly
/// what the regression suite is for. A person's cancellation does not.
pub fn is_a_failure(reason: StopReason) -> bool {
    reason.is_a_ceiling() || reason == StopReason::NeedHelp
}

fn stop_reason_word(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Done => "done",
        StopReason::MaxSteps => "max_steps",
        StopReason::Budget => "budget",
        StopReason::NeedHelp => "need_help",
        StopReason::Cancelled => "cancelled",
    }
}

fn verification_word(verification: StepVerification) -> &'static str {
    match verification {
        StepVerification::Verified => "verified",
        StepVerification::Failed => "failed",
        StepVerification::Unverifiable => "unverifiable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::run_registry::ComputerRun;
    use crate::computer::run_store::{Checkpoint, CompletedStep};
    use crate::computer::types::{ClickTarget, DesktopAction, MouseButton, Rung};
    use crate::computer::use_loop::LoopConfig;

    fn checkpoint_with(steps: Vec<CompletedStep>, reason: StopReason) -> Checkpoint {
        let mut cp = Checkpoint::new("run-1", "export the report as CSV", &LoopConfig::default());
        cp.model_calls = steps.len();
        cp.completed = steps;
        cp.stop_reason = Some(reason);
        cp
    }

    fn step(n: usize, verification: StepVerification, rung: Option<Rung>) -> CompletedStep {
        CompletedStep {
            step: n,
            action: DesktopAction::Click {
                target: ClickTarget::ElementId(format!("s1:0.{n}")),
                button: MouseButton::Left,
            },
            verification,
            result: format!("result {n}"),
            screenshot_before_ref: Some(format!("sha256:before-{n}")),
            screenshot_ref: Some(format!("sha256:after-{n}")),
            sensitive: false,
            rung,
            effect: None,
        }
    }

    #[test]
    fn a_done_or_cancelled_run_is_not_a_failure() {
        let steps = vec![step(0, StepVerification::Verified, Some(Rung::AxAction))];
        assert!(failure_of(&checkpoint_with(steps.clone(), StopReason::Done), None).is_none());
        assert!(failure_of(&checkpoint_with(steps, StopReason::Cancelled), None).is_none());
    }

    #[test]
    fn the_three_stopping_reasons_are_collected() {
        let steps = vec![step(0, StepVerification::Verified, Some(Rung::AxAction))];
        for reason in [
            StopReason::NeedHelp,
            StopReason::Budget,
            StopReason::MaxSteps,
        ] {
            let record = failure_of(&checkpoint_with(steps.clone(), reason), None)
                .unwrap_or_else(|| panic!("{reason:?} is a failure worth collecting"));
            assert_eq!(record.stop_reason, stop_reason_word(reason));
        }
    }

    #[test]
    fn the_record_points_at_the_failed_step_and_its_rung() {
        let steps = vec![
            step(0, StepVerification::Verified, Some(Rung::AxAction)),
            step(1, StepVerification::Failed, Some(Rung::Pixel)),
            step(2, StepVerification::Unverifiable, Some(Rung::Keyboard)),
        ];
        let record =
            failure_of(&checkpoint_with(steps, StopReason::NeedHelp), None).expect("a failure");

        let failed = record.failed_step.expect("the failed step");
        assert_eq!(failed.step, 1);
        assert_eq!(failed.verification, "failed");
        assert_eq!(failed.rung.as_deref(), Some("pixel"));
        assert!(failed.action.contains("click"), "{}", failed.action);

        // The last capture is the frame the run ended on, not the failed
        // step's: a person re-opening the desktop wants the final state.
        assert_eq!(record.last_screenshot_ref.as_deref(), Some("sha256:after-2"));
        assert_eq!(record.rung_distribution["ax_action"], 1);
        assert_eq!(record.rung_distribution["pixel"], 1);
        assert_eq!(record.rung_distribution["keyboard"], 1);
        assert_eq!(record.rung_distribution["page"], 0);
        assert_eq!(record.rung_distribution["unknown"], 0);
    }

    /// A run that hit its ceiling while still working has no failed step, and
    /// says so instead of naming a healthy step as the culprit.
    #[test]
    fn a_ceiling_with_no_failure_names_nothing() {
        let steps = vec![
            step(0, StepVerification::Verified, Some(Rung::AxAction)),
            step(1, StepVerification::Unverifiable, None),
        ];
        let record =
            failure_of(&checkpoint_with(steps, StopReason::MaxSteps), None).expect("a failure");
        assert!(record.failed_step.is_none());
        assert_eq!(record.rung_distribution["unknown"], 1);
        assert!(record.signature().contains("no failed step"), "{}", record.signature());
    }

    #[test]
    fn the_handoff_is_carried_verbatim() {
        let run = ComputerRun::new("run-1", "export the report as CSV");
        let history = vec![crate::computer::use_loop::StepRecord {
            step: 0,
            action: DesktopAction::Click {
                target: ClickTarget::Coordinate(crate::computer::Point { x: 1, y: 2 }),
                button: MouseButton::Left,
            },
            result: crate::computer::ActionResult::error("no effect"),
            verification: StepVerification::Failed,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
        }];
        let note =
            crate::computer::handoff::note_for(&run, &history, "stuck", StopReason::NeedHelp);
        let cp = checkpoint_with(history_to_steps(&history), StopReason::NeedHelp);
        let record = failure_of(&cp, Some(&note)).expect("a failure");

        let handoff = record.handoff.expect("the note is carried");
        assert_eq!(handoff["run_id"], "run-1");
        assert!(handoff["stuck_at"]
            .as_str()
            .unwrap_or_default()
            .contains("step 1"));
    }

    /// The signature is what dedups repeated failures; it must not carry the
    /// per-attempt facts (capture digest, result text, run id).
    #[test]
    fn the_signature_is_stable_across_attempts() {
        let mut a = failure_of(
            &checkpoint_with(
                vec![step(0, StepVerification::Failed, Some(Rung::Pixel))],
                StopReason::NeedHelp,
            ),
            None,
        )
        .unwrap();
        let b = failure_of(
            &checkpoint_with(
                vec![step(0, StepVerification::Failed, Some(Rung::Pixel))],
                StopReason::NeedHelp,
            ),
            None,
        )
        .unwrap();
        assert_eq!(a.signature(), b.signature());

        // The attempt-specific evidence differs, and does not move it.
        a.last_screenshot_ref = Some("sha256:one".to_string());
        a.steps_completed = 7;
        assert_eq!(a.signature(), b.signature());
    }

    fn history_to_steps(history: &[crate::computer::use_loop::StepRecord]) -> Vec<CompletedStep> {
        history
            .iter()
            .map(|r| CompletedStep {
                step: r.step,
                action: r.action.clone(),
                verification: r.verification,
                result: r.result.message.clone(),
                screenshot_before_ref: None,
                screenshot_ref: None,
                sensitive: r.sensitive,
                rung: None,
                effect: None,
            })
            .collect()
    }
}
