//! What one computer-use run may spend, and when it is worth an image.
//!
//! The loop's default shape is expensive: thirty steps, one model call each,
//! one full-screen image each. Two existing guards do not cover it —
//! `CostGuard` limits an *agent* per day, and `materialize_history` limits one
//! *request* to eight images, which a thirty-step run satisfies on every step.
//! Neither answers "how many images did this one goal cost me".
//!
//! So a run carries its own counters ([`RunBudget`]) and its own verdict on
//! why it stopped ([`StopReason`]). The distinction matters more than the
//! limit: `max_steps` means "it may finish if given more room", `budget` means
//! "it was still working when the money ran out", and `done` means "it
//! finished". Collapsing them makes the run log useless for deciding which
//! knob to turn.
//!
//! [`plan_perception`] is the other half: a step is text-first, and an image
//! is attached only when the text cannot carry the question. Three conditions
//! trigger that, each one a case where the tree is known to be insufficient
//! rather than merely suspected to be.

use serde::{Deserialize, Serialize};

use crate::config::RunBudgetConfig;

/// Why a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The decision maker said the goal was met.
    Done,
    /// The step ceiling was reached while still making progress.
    MaxSteps,
    /// A cost ceiling was reached while still making progress.
    Budget,
    /// The loop could not continue and asked for a person.
    NeedHelp,
    /// Someone cancelled it.
    Cancelled,
}

impl StopReason {
    /// Whether more room would plausibly have let the run finish.
    ///
    /// The two ceiling reasons do; the other three are answers, not limits.
    /// This is what a caller reads to decide whether to retry with a larger
    /// budget or to look at a failure.
    pub fn is_a_ceiling(self) -> bool {
        matches!(self, StopReason::MaxSteps | StopReason::Budget)
    }

    /// How to name the reason in a sentence for a person.
    pub fn describe(self) -> &'static str {
        match self {
            StopReason::Done => "done",
            StopReason::MaxSteps => "reached the step limit",
            StopReason::Budget => "reached the budget",
            StopReason::NeedHelp => "needs a person",
            StopReason::Cancelled => "cancelled",
        }
    }
}

/// Which ceiling was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    /// Model calls.
    Calls,
    /// Images attached to those calls.
    Images,
}

impl BudgetKind {
    /// How to name it in a sentence for a person.
    pub fn describe(self) -> &'static str {
        match self {
            BudgetKind::Calls => "model calls",
            BudgetKind::Images => "images",
        }
    }
}

/// A refusal from [`RunBudget`], carrying why and how much was left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhausted {
    pub kind: BudgetKind,
    pub spent: usize,
    pub limit: usize,
}

impl std::fmt::Display for Exhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} budget reached ({}/{})", self.kind.describe(), self.spent, self.limit)
    }
}

/// Counters for one run.
#[derive(Debug, Clone)]
pub struct RunBudget {
    limits: RunBudgetConfig,
    model_calls: usize,
    images_attached: usize,
    tokens_in: u64,
    tokens_out: u64,
    exhausted: Option<Exhausted>,
}

impl RunBudget {
    pub fn new(limits: RunBudgetConfig) -> Self {
        Self {
            limits,
            model_calls: 0,
            images_attached: 0,
            tokens_in: 0,
            tokens_out: 0,
            exhausted: None,
        }
    }

    /// A budget with no ceilings, for tests and for callers that manage cost
    /// elsewhere. Every check passes; the counters still run.
    pub fn unlimited() -> Self {
        Self::new(RunBudgetConfig {
            max_calls_per_run: usize::MAX,
            max_images_per_run: usize::MAX,
        })
    }

    /// Seed the counters from a checkpoint a previous process wrote.
    ///
    /// A resumed run continues the same spend, not a fresh allowance: handing it
    /// an empty budget would let one restart double what the caller permitted.
    pub fn restore(limits: RunBudgetConfig, model_calls: usize, images_attached: usize) -> Self {
        Self {
            limits,
            model_calls,
            images_attached,
            tokens_in: 0,
            tokens_out: 0,
            exhausted: None,
        }
    }

    pub fn limits(&self) -> &RunBudgetConfig {
        &self.limits
    }

    pub fn model_calls(&self) -> usize {
        self.model_calls
    }

    pub fn images_attached(&self) -> usize {
        self.images_attached
    }

    /// Why the run is out of budget, once it is. Sticky: the first ceiling hit
    /// is the one reported, so a later call to another ceiling does not
    /// rewrite the reason.
    pub fn exhausted(&self) -> Option<Exhausted> {
        self.exhausted
    }

    /// Whether a model call can be made at all.
    ///
    /// Consults the sticky `exhausted` flag as well as the count: once any
    /// ceiling has been hit the run is over, and answering "yes, you have
    /// calls left" after an image overshoot would invite a caller to keep
    /// going with a budget that has already been reported as spent.
    pub fn can_call(&self) -> bool {
        self.exhausted.is_none() && self.model_calls < self.limits.max_calls_per_run
    }

    /// Whether one more image fits.
    pub fn can_attach_image(&self) -> bool {
        self.exhausted.is_none() && self.images_attached < self.limits.max_images_per_run
    }

    /// Record a model call, or refuse it.
    ///
    /// Refusing is a decision, not an error: the caller turns it into a
    /// `budget` stop with the completed steps attached, rather than silently
    /// ending the run.
    pub fn record_call(&mut self) -> Result<(), Exhausted> {
        if let Some(exhausted) = self.exhausted {
            return Err(exhausted);
        }
        if self.model_calls >= self.limits.max_calls_per_run {
            let exhausted = Exhausted {
                kind: BudgetKind::Calls,
                spent: self.model_calls,
                limit: self.limits.max_calls_per_run,
            };
            self.exhausted = Some(exhausted);
            return Err(exhausted);
        }
        self.model_calls += 1;
        Ok(())
    }

    /// Record `n` attached images, or refuse them all.
    ///
    /// All-or-nothing on purpose: half a multi-image request is a different
    /// question to the model than the one the caller meant to ask.
    pub fn record_images(&mut self, n: usize) -> Result<(), Exhausted> {
        if n == 0 {
            return Ok(());
        }
        if let Some(exhausted) = self.exhausted {
            return Err(exhausted);
        }
        let after = self.images_attached.saturating_add(n);
        if after > self.limits.max_images_per_run {
            let exhausted = Exhausted {
                kind: BudgetKind::Images,
                spent: self.images_attached,
                limit: self.limits.max_images_per_run,
            };
            self.exhausted = Some(exhausted);
            return Err(exhausted);
        }
        self.images_attached = after;
        Ok(())
    }

    /// Add the token counts a provider reported.
    ///
    /// Tokens are *recorded*, not capped: the money is already spent by the
    /// time they are known, so refusing them would only lose the accounting.
    /// The call and image ceilings are what prevent the spend.
    pub fn record_tokens(&mut self, input: u64, output: u64) {
        self.tokens_in = self.tokens_in.saturating_add(input);
        self.tokens_out = self.tokens_out.saturating_add(output);
    }

    /// The line written to the run summary.
    pub fn summary(&self, steps: usize, stop_reason: StopReason) -> RunSummary {
        RunSummary {
            steps,
            model_calls: self.model_calls,
            images_attached: self.images_attached,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            stop_reason,
        }
    }
}

/// The record of what one run cost, kept with its result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub steps: usize,
    pub model_calls: usize,
    pub images_attached: usize,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub stop_reason: StopReason,
}

/// Why a step needs a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageTrigger {
    /// The accessibility tree was cut by a depth or element limit, so the
    /// semantic path cannot name what is on screen.
    TreeIncomplete,
    /// The accessibility tree could not be read at all: no tree, not a short
    /// one. The distinction matters to the sentence the model is shown — a cut
    /// tree means "look deeper", an unreadable one means "the picture is all
    /// there is" — and it decides whether re-reading is worth a step.
    TreeUnreadable,
    /// The last action did not verify — either it plainly did not land, or
    /// nothing could check it. Both mean the text says nothing trustworthy
    /// about what happened.
    LastStepNotVerified,
    /// Two or more failures in a row: the current channel is not working.
    RepeatedFailures,
}

impl ImageTrigger {
    pub fn describe(self) -> &'static str {
        match self {
            ImageTrigger::TreeIncomplete => "the accessibility tree is incomplete",
            ImageTrigger::TreeUnreadable => "the accessibility tree could not be read",
            ImageTrigger::LastStepNotVerified => "the last action was not verified",
            ImageTrigger::RepeatedFailures => "repeated failures",
        }
    }
}

/// What a step should read, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerceptionPlan {
    /// Attach an image to the decision prompt.
    pub attach_image: bool,
    /// Which condition asked for one. `None` when no image is attached.
    pub trigger: Option<ImageTrigger>,
    /// The tree's completeness, carried through so the caller can widen the
    /// line budget when the tree is the problem — an image is not the only
    /// answer to a truncated read.
    pub tree_complete: bool,
}

/// Inputs to the perception decision.
#[derive(Debug, Clone, Copy)]
pub struct PerceptionInputs {
    /// Whether the accessibility tree was read in full.
    pub tree_complete: bool,
    /// Whether the read failed outright, producing no tree.
    ///
    /// Implies `!tree_complete`, and is carried separately because "the tree
    /// was cut" and "there is no tree" call for different words in the prompt.
    pub tree_read_failed: bool,
    /// The previous step's verification, if there was one.
    pub last_verified: Option<bool>,
    /// Consecutive failed steps so far.
    pub consecutive_failures: usize,
    /// Whether the model routed to can accept an image at all.
    pub model_supports_vision: bool,
    /// Whether the run has image budget left.
    pub image_budget_available: bool,
}

/// Decide whether this step gets a picture.
///
/// Text first, because the accessibility tree is the *better* input when it is
/// whole: it gives roles, labels and stable element ids, and a model that
/// clicks by id is far more reliable than one estimating a coordinate from
/// pixels. An image is attached when the text is known to be insufficient —
/// not by default, and not as a refresh.
///
/// The three triggers are ordered by how much they tell the model. A
/// truncated tree is a fact about the input; an unverified step is a fact
/// about the last action; repeated failures are a fact about the approach.
/// The first one that applies is reported, so the run log says what actually
/// prompted the spend.
pub fn plan_perception(inputs: PerceptionInputs) -> PerceptionPlan {
    let no_image = |trigger_reason: Option<ImageTrigger>| PerceptionPlan {
        attach_image: false,
        trigger: trigger_reason,
        tree_complete: inputs.tree_complete,
    };

    // A model without vision cannot use an image and may reject the request
    // outright. Attaching one would spend tokens to be told nothing.
    if !inputs.model_supports_vision {
        return no_image(None);
    }
    // Out of images: keep going on text rather than stopping. The run may
    // still finish, and the budget ceiling is reported separately if it does
    // not.
    if !inputs.image_budget_available {
        return no_image(None);
    }

    if !inputs.tree_complete {
        return PerceptionPlan {
            attach_image: true,
            trigger: Some(if inputs.tree_read_failed {
                ImageTrigger::TreeUnreadable
            } else {
                ImageTrigger::TreeIncomplete
            }),
            tree_complete: false,
        };
    }
    if inputs.last_verified == Some(false) {
        return PerceptionPlan {
            attach_image: true,
            trigger: Some(ImageTrigger::LastStepNotVerified),
            tree_complete: true,
        };
    }
    if inputs.consecutive_failures >= 2 {
        return PerceptionPlan {
            attach_image: true,
            trigger: Some(ImageTrigger::RepeatedFailures),
            tree_complete: true,
        };
    }
    no_image(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(calls: usize, images: usize) -> RunBudget {
        RunBudget::new(RunBudgetConfig {
            max_calls_per_run: calls,
            max_images_per_run: images,
        })
    }

    fn inputs() -> PerceptionInputs {
        PerceptionInputs {
            tree_complete: true,
            tree_read_failed: false,
            last_verified: Some(true),
            consecutive_failures: 0,
            model_supports_vision: true,
            image_budget_available: true,
        }
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let cfg = RunBudgetConfig::default();
        assert_eq!(cfg.max_calls_per_run, 40);
        assert_eq!(cfg.max_images_per_run, 15);
    }

    /// The headline claim: a healthy step is text-only.
    #[test]
    fn a_verified_step_with_a_whole_tree_gets_no_image() {
        let plan = plan_perception(inputs());
        assert!(!plan.attach_image);
        assert_eq!(plan.trigger, None);
        assert!(plan.tree_complete);
    }

    #[test]
    fn a_truncated_tree_asks_for_an_image() {
        let plan = plan_perception(PerceptionInputs {
            tree_complete: false,
            ..inputs()
        });
        assert!(plan.attach_image);
        assert_eq!(plan.trigger, Some(ImageTrigger::TreeIncomplete));
        assert!(!plan.tree_complete);
    }

    /// A tree that could not be read at all is not a truncated one. Both ask
    /// for the picture, but the model is told a different thing — "look
    /// deeper" is wrong advice when there was nothing to look at.
    #[test]
    fn an_unreadable_tree_is_named_as_unreadable_not_truncated() {
        let plan = plan_perception(PerceptionInputs {
            tree_complete: false,
            tree_read_failed: true,
            ..inputs()
        });
        assert!(plan.attach_image);
        assert_eq!(plan.trigger, Some(ImageTrigger::TreeUnreadable));
        assert_ne!(
            ImageTrigger::TreeUnreadable.describe(),
            ImageTrigger::TreeIncomplete.describe(),
            "the two sentences must not be interchangeable"
        );
    }

    #[test]
    fn an_unverified_step_asks_for_an_image() {
        let plan = plan_perception(PerceptionInputs {
            last_verified: Some(false),
            ..inputs()
        });
        assert!(plan.attach_image);
        assert_eq!(plan.trigger, Some(ImageTrigger::LastStepNotVerified));
    }

    /// `None` is "nothing was checked", which is not a failure. Attaching an
    /// image on every `Unverifiable` would make the cheap path the exception.
    #[test]
    fn an_unchecked_step_alone_does_not_ask_for_an_image() {
        let plan = plan_perception(PerceptionInputs {
            last_verified: None,
            ..inputs()
        });
        assert!(!plan.attach_image, "{plan:?}");
    }

    #[test]
    fn repeated_failures_ask_for_an_image() {
        let plan = plan_perception(PerceptionInputs {
            consecutive_failures: 2,
            ..inputs()
        });
        assert!(plan.attach_image);
        assert_eq!(plan.trigger, Some(ImageTrigger::RepeatedFailures));
    }

    #[test]
    fn one_failure_alone_does_not_switch_channels() {
        let plan = plan_perception(PerceptionInputs {
            consecutive_failures: 1,
            ..inputs()
        });
        assert!(!plan.attach_image);
    }

    /// The tree says more than the trigger order suggests when it is whole and
    /// the last step plainly failed — but a truncated tree still wins, because
    /// it changes what the model can even be asked.
    #[test]
    fn the_most_informative_trigger_is_reported() {
        let plan = plan_perception(PerceptionInputs {
            tree_complete: false,
            last_verified: Some(false),
            consecutive_failures: 5,
            ..inputs()
        });
        assert_eq!(plan.trigger, Some(ImageTrigger::TreeIncomplete));
    }

    /// A model without vision must not be sent an image: it burns tokens and
    /// may be rejected outright, and the plan's default provider is one.
    #[test]
    fn a_model_without_vision_gets_no_image_however_bad_things_are() {
        let plan = plan_perception(PerceptionInputs {
            tree_complete: false,
            tree_read_failed: false,
            last_verified: Some(false),
            consecutive_failures: 9,
            model_supports_vision: false,
            image_budget_available: true,
        });
        assert!(!plan.attach_image, "{plan:?}");
        assert_eq!(plan.trigger, None, "nothing was attached, so nothing prompted it");
    }

    /// Out of image budget is not a reason to stop: the run continues on text.
    #[test]
    fn no_image_budget_falls_back_to_text_rather_than_stopping() {
        let plan = plan_perception(PerceptionInputs {
            tree_complete: false,
            image_budget_available: false,
            ..inputs()
        });
        assert!(!plan.attach_image);
    }

    #[test]
    fn calls_are_counted_and_then_refused() {
        let mut b = budget(3, 10);
        for _ in 0..3 {
            assert!(b.record_call().is_ok());
        }
        assert_eq!(b.model_calls(), 3);
        assert!(!b.can_call());
        let err = b.record_call().expect_err("the fourth call exceeds 3");
        assert_eq!(err.kind, BudgetKind::Calls);
        assert_eq!((err.spent, err.limit), (3, 3));
        // Sticky, and the reason is reported once.
        assert_eq!(b.exhausted(), Some(err));
    }

    /// A ceiling on one axis does not silently lift the other: once the run is
    /// over, both answers are "no".
    #[test]
    fn an_exhausted_run_answers_no_to_everything() {
        let mut b = budget(10, 1);
        b.record_images(1).unwrap();
        b.record_images(1).unwrap_err();
        assert!(b.exhausted().is_some());
        assert!(!b.can_call(), "calls remained, but the run is over");
        assert!(!b.can_attach_image());
    }

    #[test]
    fn images_are_all_or_nothing() {
        let mut b = budget(10, 5);
        assert!(b.record_images(4).is_ok());
        assert_eq!(b.images_attached(), 4);
        // Two more would make six; neither is attached.
        let err = b.record_images(2).expect_err("would exceed 5");
        assert_eq!(err.kind, BudgetKind::Images);
        assert_eq!(b.images_attached(), 4, "a partial attach is a different question");
    }

    /// An overshoot ends the image budget rather than leaving it dangling.
    ///
    /// The alternative — refuse the two, allow a later one — would let a run
    /// keep asking after it had already been told it was over, and the final
    /// `stop_reason` would then depend on how many single-image steps happened
    /// to follow. One refusal, one reason.
    #[test]
    fn an_overshoot_ends_the_image_budget() {
        let mut b = budget(10, 5);
        b.record_images(4).unwrap();
        b.record_images(2).unwrap_err();

        assert!(!b.can_attach_image());
        let err = b.record_images(1).expect_err("the budget is already over");
        assert_eq!(err.kind, BudgetKind::Images);
        assert_eq!(b.summary(3, StopReason::Budget).images_attached, 4);
    }

    #[test]
    fn zero_images_is_not_a_call() {
        let mut b = budget(1, 0);
        assert!(b.record_images(0).is_ok());
        assert_eq!(b.images_attached(), 0);
        assert!(b.exhausted().is_none());
    }

    /// The first ceiling hit is the reason. Otherwise a run that ran out of
    /// images and then touched the call ceiling would report the wrong one.
    #[test]
    fn the_first_ceiling_is_the_one_reported() {
        let mut b = budget(2, 1);
        b.record_call().unwrap();
        b.record_images(1).unwrap();
        let first = b.record_images(1).expect_err("image ceiling");
        assert_eq!(first.kind, BudgetKind::Images);
        // The call ceiling is not reached, but the run is already over.
        let second = b.record_call().expect_err("stays exhausted");
        assert_eq!(second, first);
    }

    /// Tokens are recorded even after the run is out of budget: the money is
    /// spent, and losing the count makes the summary a lie.
    #[test]
    fn tokens_still_accumulate_after_the_ceiling() {
        let mut b = budget(1, 1);
        b.record_call().unwrap();
        b.record_tokens(100, 20);
        b.record_call().unwrap_err();
        b.record_tokens(50, 10);
        let sum = b.summary(4, StopReason::Budget);
        assert_eq!((sum.tokens_in, sum.tokens_out), (150, 30));
        assert_eq!(sum.stop_reason, StopReason::Budget);
        assert_eq!(sum.model_calls, 1);
    }

    #[test]
    fn a_summary_carries_the_four_way_stop_reason() {
        let b = budget(10, 10);
        for reason in [
            StopReason::Done,
            StopReason::MaxSteps,
            StopReason::Budget,
            StopReason::NeedHelp,
            StopReason::Cancelled,
        ] {
            assert_eq!(b.summary(2, reason).stop_reason, reason);
        }
        assert!(StopReason::Budget.is_a_ceiling());
        assert!(StopReason::MaxSteps.is_a_ceiling());
        assert!(!StopReason::Done.is_a_ceiling());
        assert!(!StopReason::NeedHelp.is_a_ceiling());
    }

    /// A ceiling is not a failure and must not be reported as one: the run
    /// was still working when it ran out.
    #[test]
    fn a_ceiling_stop_is_not_reported_as_done() {
        let mut b = budget(1, 1);
        b.record_call().unwrap();
        let sum = b.summary(1, StopReason::Budget);
        assert_ne!(sum.stop_reason, StopReason::Done);
        assert!(sum.stop_reason.is_a_ceiling());
    }

    #[test]
    fn an_unlimited_budget_never_refuses() {
        let mut b = RunBudget::unlimited();
        for _ in 0..1000 {
            b.record_call().unwrap();
        }
        b.record_images(500).unwrap();
        assert!(b.exhausted().is_none());
    }

    #[test]
    fn the_exhaustion_message_names_what_ran_out() {
        let mut b = budget(1, 1);
        b.record_call().unwrap();
        let err = b.record_call().unwrap_err();
        let text = err.to_string();
        assert!(text.contains("model calls"), "{text}");
        assert!(text.contains("1/1"), "{text}");
    }
}
