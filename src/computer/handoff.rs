//! What a person is told when a run stops without finishing.
//!
//! The loop's old answer was a `NeedHelp` string — usually "Stuck after 5
//! consecutive failures. Last error: …". That names the *state* of the run and
//! nothing a person can act on: not what was accomplished, not what is on
//! screen now, not what was tried, not what to do next. Whoever picks the
//! screen up had to reread the whole history to find out.
//!
//! This module turns a finished run into that note, in four parts:
//!
//! 1. **What got done** — the steps that verified, in order.
//! 2. **Where it stopped** — the step that failed or the ceiling it hit.
//! 3. **What was tried** — the failures, with their errors, so a person does
//!    not repeat them.
//! 4. **What to do next** — advice derived from *why* it stopped, not a
//!    generic "please check".
//!
//! The note is a real handoff, so it is also what gets posted to the approval
//! queue: a person who is asked to take over needs the same four things a
//! person reading a run log does.

use serde::{Deserialize, Serialize};

use crate::computer::run_budget::StopReason;
use crate::computer::run_registry::{ComputerRun, RunPhase};
use crate::computer::use_loop::{StepRecord, StepVerification};

/// The four-part handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffNote {
    pub run_id: String,
    pub goal: String,
    /// What was accomplished, one line per verified step.
    pub completed: Vec<String>,
    /// Where it stopped, in one sentence.
    pub stuck_at: String,
    /// Attempts that did not work, so they are not repeated.
    pub tried: Vec<String>,
    /// What a person should do next.
    pub next: String,
    pub stop_reason: StopReason,
}

impl HandoffNote {
    /// The note as plain text, for a chat surface or a notification.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Computer run stopped: {}\n", self.goal));
        out.push_str(&format!("Where: {}\n", self.stuck_at));
        if self.completed.is_empty() {
            out.push_str("Done: nothing yet\n");
        } else {
            out.push_str(&format!("Done ({} steps):\n", self.completed.len()));
            for line in &self.completed {
                out.push_str(&format!("  {line}\n"));
            }
        }
        if !self.tried.is_empty() {
            out.push_str("Tried and failed:\n");
            for line in &self.tried {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out.push_str(&format!("Next: {}\n", self.next));
        out
    }
}

/// Build the note for a finished run.
pub fn note_for(
    run: &ComputerRun,
    history: &[StepRecord],
    message: &str,
    stop_reason: StopReason,
) -> HandoffNote {
    let mut completed = Vec::new();
    let mut tried = Vec::new();
    for record in history {
        let line = describe(record);
        match record.verification {
            StepVerification::Verified => completed.push(line),
            // Both failures and unchecked steps count as "tried": an action
            // nobody could confirm is not an accomplished step, and leaving it
            // out of the tried list would invite a person to repeat it.
            StepVerification::Failed | StepVerification::Unverifiable => tried.push(line),
        }
    }

    let stuck_at = if let Some(last) = history.last() {
        format!(
            "step {} — {} ({})",
            last.step + 1,
            crate::computer::driver::describe_action(&last.action),
            last.result.message
        )
    } else {
        "before the first step".to_string()
    };

    HandoffNote {
        run_id: run.run_id.clone(),
        goal: run.goal.clone(),
        completed,
        stuck_at,
        tried,
        next: advice(stop_reason, message),
        stop_reason,
    }
}

fn describe(record: &StepRecord) -> String {
    format!(
        "{}. {} → {}",
        record.step + 1,
        crate::computer::driver::describe_action(&record.action),
        truncate(&record.result.message, 120)
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

/// Advice keyed on why the run stopped.
///
/// Deliberately specific to the reason: "please take a look" is what a note
/// says when its author did not know why it stopped, and it wastes the one
/// thing a person has to spend — attention on the right thing.
fn advice(reason: StopReason, message: &str) -> String {
    match reason {
        StopReason::Done => "Nothing — the run finished on its own.".to_string(),
        StopReason::MaxSteps => format!(
            "It was still making progress when the step ceiling stopped it ({message}). \
             Retry with a larger `computer.max_steps`, or split the goal into two."
        ),
        StopReason::Budget => format!(
            "It hit a cost ceiling, not a wall ({message}). Retry with a larger \
             `computer.budget.max_calls_per_run` / `max_images_per_run`, or narrow the goal."
        ),
        StopReason::NeedHelp => {
            if message.to_ascii_lowercase().contains("not a decision") {
                "The decision model could not produce a usable instruction. Check which model \
                 is configured for computer use — a model without tool/JSON discipline will \
                 keep failing here — or take the screen over yourself."
                    .to_string()
            } else {
                format!(
                    "Take the screen over from the state described above, or correct the \
                     obstacle it named and re-run. The last error was: {message}"
                )
            }
        }
        StopReason::Cancelled => "Nothing — it was cancelled on purpose.".to_string(),
    }
}

/// The phase a note implies.
pub fn phase_of(reason: StopReason) -> RunPhase {
    match reason {
        StopReason::Done => RunPhase::Done,
        StopReason::Cancelled => RunPhase::Cancelled,
        _ => RunPhase::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::{ClickTarget, DesktopAction, MouseButton};
    use crate::computer::ActionResult;

    fn record(step: usize, verified: StepVerification, message: &str) -> StepRecord {
        StepRecord {
            step,
            action: DesktopAction::Click {
                target: ClickTarget::ElementId(format!("s1:0.{step}")),
                button: MouseButton::Left,
            },
            result: if verified == StepVerification::Verified {
                ActionResult::success(message.to_string())
            } else {
                ActionResult::error(message.to_string())
            },
            verification: verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
        }
    }

    #[tokio::test]
    async fn the_note_separates_done_from_tried() {
        let run = ComputerRun::new("r1", "open the calculator");
        let history = vec![
            record(0, StepVerification::Verified, "launched"),
            record(1, StepVerification::Failed, "element stale"),
            record(2, StepVerification::Unverifiable, "no way to check"),
        ];
        let note = note_for(&run, &history, "stuck", StopReason::NeedHelp);

        assert_eq!(note.completed.len(), 1, "{:?}", note.completed);
        assert!(note.completed[0].contains("launched"));
        // Both a failure and an unconfirmable action go in `tried`: neither was
        // accomplished, so a person must not be invited to assume they were.
        assert_eq!(note.tried.len(), 2, "{:?}", note.tried);
        assert!(note.tried[0].contains("element stale"));
    }

    #[tokio::test]
    async fn the_note_names_where_it_stopped() {
        let run = ComputerRun::new("r1", "g");
        let history = vec![record(0, StepVerification::Failed, "window not found")];
        let note = note_for(&run, &history, "boom", StopReason::NeedHelp);
        assert!(note.stuck_at.contains("step 1"), "{}", note.stuck_at);
        assert!(note.stuck_at.contains("window not found"), "{}", note.stuck_at);
    }

    /// A run that never got a step says so, rather than claiming "step 1".
    #[tokio::test]
    async fn a_run_with_no_steps_says_before_the_first_step() {
        let run = ComputerRun::new("r1", "g");
        let note = note_for(&run, &[], "no provider", StopReason::NeedHelp);
        assert_eq!(note.stuck_at, "before the first step");
        assert!(note.completed.is_empty());
        assert!(note.tried.is_empty());
    }

    /// Each ceiling gets its own advice; a generic message is the failure mode
    /// this module exists to avoid.
    #[test]
    fn the_advice_depends_on_why_it_stopped() {
        let steps = advice(StopReason::MaxSteps, "Max steps (30) reached");
        assert!(steps.contains("max_steps"), "{steps}");
        assert!(steps.contains("larger"), "{steps}");

        let budget =
            advice(StopReason::Budget, "stopped at step 12: model calls budget reached (40/40)");
        assert!(budget.contains("max_calls_per_run"), "{budget}");

        let stuck = advice(StopReason::NeedHelp, "window not found");
        assert!(stuck.contains("window not found"), "{stuck}");

        let unreadable = advice(
            StopReason::NeedHelp,
            "the decision model kept answering with something that is not a decision",
        );
        assert!(unreadable.contains("JSON discipline"), "{unreadable}");

        assert_ne!(steps, budget);
        assert_ne!(stuck, unreadable);
    }

    #[tokio::test]
    async fn the_rendered_note_contains_all_four_parts() {
        let run = ComputerRun::new("r1", "open the calculator");
        let history = vec![
            record(0, StepVerification::Verified, "launched"),
            record(1, StepVerification::Failed, "stale element"),
        ];
        let text = note_for(&run, &history, "stuck", StopReason::NeedHelp).render();
        assert!(text.contains("open the calculator"), "{text}");
        assert!(text.contains("Done (1 steps)"), "{text}");
        assert!(text.contains("Tried and failed"), "{text}");
        assert!(text.contains("Where: step 2"), "{text}");
        assert!(text.contains("Next:"), "{text}");
    }

    #[test]
    fn a_completed_run_needs_no_person() {
        assert!(advice(StopReason::Done, "ok").contains("Nothing"));
        assert_eq!(phase_of(StopReason::Done), RunPhase::Done);
        assert_eq!(phase_of(StopReason::Cancelled), RunPhase::Cancelled);
        assert_eq!(phase_of(StopReason::Budget), RunPhase::Failed);
    }
}
