//! The ladder in front of an action that is being repeated.
//!
//! An agent on a phone fails in three shapes, and they are not the same shape:
//!
//! - **The same action, over and over.** A tap that worked once is tried again
//!   because the model misread the result, and again, and again.
//! - **No effect.** The action differs each time but the screen does not move:
//!   a button that is disabled, a field that will not focus, a scroll that has
//!   hit the bottom.
//! - **Going in circles.** Two or three screens cycling: open the sheet, close
//!   the sheet, open the sheet.
//!
//! Mika answers all three with one `GuardRules` ladder — graded nudges, then a
//! stop, with the model told *which* rung it is on. This is that ladder, with
//! the same numbers and the same ordering: a note the model can still act on
//! before a refusal it cannot argue with.
//!
//! # Why the counts live in a process-wide bucket
//!
//! The guard has to survive between tool calls, and the thing it is counting is
//! "what this conversation has been doing to this target" — not what one
//! `android_input` call did. That is the same lifetime [`Pacing`] already has,
//! keyed the same way and pruned the same way, for the same reason: one phone
//! is one place, and two turns driving it share what happens on it.
//!
//! A bucket that has gone quiet is forgotten, so a conversation that returns to
//! an app an hour later starts at the bottom rung. The window is the guard's
//! own; [`Pacing`]'s 30 minutes is about how fast a person works, which is a
//! different question.
//!
//! # What it is not
//!
//! It does not decide whether an action is *right* — [`risk`](super::risk) and
//! [`gate`](super::gate) do that, before this ever sees it. It only notices
//! that whatever is being attempted is not working, and says so in time.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What the screen did after an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The screen was different afterwards.
    Changed,
    /// The screen was the same, twice, after waiting.
    Unchanged,
    /// The screen could not be re-read, so nothing can be said.
    Unknown,
}

impl Outcome {
    /// Whether this counts as "that did not work".
    ///
    /// Only `Unchanged` does. `Unknown` is not evidence in either direction —
    /// a screen that cannot be read is not a screen that did not move, and
    /// counting it would punish the apps whose trees are hardest to read.
    fn is_no_effect(self) -> bool {
        matches!(self, Self::Unchanged)
    }
}

/// What the caller should do with the action it is about to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to say.
    Clear,
    /// Go ahead, but tell the model this is rung N of the ladder.
    Note(String),
    /// Do not take it. The message says what to do instead.
    Refuse(String),
}

impl Verdict {
    /// The note to attach to a result, if any.
    pub fn note(&self) -> Option<&str> {
        match self {
            Self::Note(text) => Some(text),
            _ => None,
        }
    }

    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Refuse(_))
    }
}

/// One action this conversation took on one target.
#[derive(Debug, Clone)]
struct Step {
    signature: String,
    outcome: Outcome,
    at: Instant,
}

/// A conversation's history for one target.
#[derive(Debug, Default)]
struct Bucket {
    steps: VecDeque<Step>,
    last_touch: Option<Instant>,
}

/// How long a quiet target is remembered.
const WINDOW: Duration = Duration::from_secs(20 * 60);

/// The most steps one bucket keeps, whatever the window says.
///
/// The ladder's top rung is a dozen; keeping more than that would only measure
/// a loop that has already been stopped.
const MAX_STEPS: usize = 32;

/// Repeat thresholds: note, insist, stop.
const REPEAT_NOTICE: usize = 3;
const REPEAT_INSIST: usize = 6;
const REPEAT_STOP: usize = 12;

/// Consecutive no-effect thresholds.
const NO_EFFECT_NOTICE: usize = 3;
const NO_EFFECT_INSIST: usize = 6;
const NO_EFFECT_STOP: usize = 9;

/// The shortest cycle worth calling a circle, and when it stops being one.
const CIRCLE_NOTICE: usize = 6;
const CIRCLE_STOP: usize = 12;

/// The guard's history, one bucket per target.
#[derive(Debug, Default)]
pub struct Guard {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Guard {
    /// The process-wide history.
    ///
    /// Process-wide because a bucket outlives one tool call and the ladder
    /// spans many of them. The bucket *key* is where the scope lives: it names
    /// one conversation on one display, so two turns do not stop each other
    /// for doing different things to the same phone, while one turn cannot
    /// reset its own count by reconnecting.
    pub fn global() -> &'static Guard {
        static GUARD: std::sync::OnceLock<Guard> = std::sync::OnceLock::new();
        GUARD.get_or_init(Guard::default)
    }

    /// The bucket key for one conversation acting on one target.
    ///
    /// `target` is the display, not the app: a loop can change apps and still
    /// be a loop, and "the screen is stuck" is a fact about the screen.
    pub fn bucket_key(conversation_id: &str, target: &str) -> String {
        format!("{conversation_id}@{target}")
    }

    /// What to do about `signature`, given what has happened on `key`.
    ///
    /// Called *before* dispatch, so a refusal is a refusal to act rather than a
    /// complaint afterwards.
    pub fn check(&self, key: &str, signature: &str) -> Verdict {
        let mut buckets = self.lock();
        let now = Instant::now();
        let bucket = buckets.entry(key.to_string()).or_default();
        Self::prune(bucket, now);
        Self::judge(bucket, signature)
    }

    /// Record that `signature` ran and what the screen did.
    pub fn record(&self, key: &str, signature: &str, outcome: Outcome) {
        let mut buckets = self.lock();
        let now = Instant::now();
        let bucket = buckets.entry(key.to_string()).or_default();
        Self::prune(bucket, now);
        bucket.last_touch = Some(now);
        bucket.steps.push_back(Step {
            signature: signature.to_string(),
            outcome,
            at: now,
        });
        while bucket.steps.len() > MAX_STEPS {
            bucket.steps.pop_front();
        }
    }

    /// Forget a target — a task that finished, or one told to start over.
    pub fn forget(&self, key: &str) {
        self.lock().remove(key);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Bucket>> {
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn prune(bucket: &mut Bucket, now: Instant) {
        while let Some(front) = bucket.steps.front() {
            if now.saturating_duration_since(front.at) > WINDOW {
                bucket.steps.pop_front();
            } else {
                break;
            }
        }
        if bucket.steps.is_empty() {
            bucket.last_touch = None;
        }
    }

    /// The ladder itself: the strongest rung the history has reached.
    ///
    /// Strongest first, so a stop is never reported as a note.
    fn judge(bucket: &Bucket, signature: &str) -> Verdict {
        let steps: Vec<&Step> = bucket.steps.iter().collect();
        let repeats = steps.iter().filter(|s| s.signature == signature).count();
        let streak = trailing_no_effect(&steps);
        let cycle = circle_period(&steps);

        if repeats >= REPEAT_STOP {
            return Verdict::Refuse(format!(
                "Refused: this is the same action for the {repeats}th time and it is not \
                 getting anywhere. Stop repeating it — read the screen with android_observe, \
                 work out why it is not doing what you expect, and do something else. If the \
                 task cannot be finished, say what is blocking it."
            ));
        }
        if streak >= NO_EFFECT_STOP {
            return Verdict::Refuse(format!(
                "Refused: {streak} actions in a row changed nothing on the screen. Repeating \
                 this will not change that. Read the screen with android_observe, and if it \
                 still does not respond, stop and report what is blocking the task."
            ));
        }
        if let Some(period) = cycle.filter(|_| steps.len() >= CIRCLE_STOP) {
            return Verdict::Refuse(format!(
                "Refused: the last {} actions are cycling through {period} screens without \
                 progress. This is a loop. Read the screen with android_observe, decide which \
                 step of the cycle is the wrong one, and break it — or stop and report what is \
                 blocking the task.",
                steps.len()
            ));
        }

        if repeats >= REPEAT_INSIST {
            return Verdict::Note(format!(
                "You have done this same action {repeats} times. It is not producing progress. \
                 Read the screen before doing it again."
            ));
        }
        if streak >= NO_EFFECT_INSIST {
            return Verdict::Note(format!(
                "The last {streak} actions changed nothing on the screen. Whatever you are \
                 attempting is not reaching it — read the screen with android_observe and \
                 reconsider the target rather than retrying."
            ));
        }
        if let Some(period) = cycle.filter(|_| steps.len() >= CIRCLE_NOTICE) {
            return Verdict::Note(format!(
                "The last {} actions are cycling through {period} screens. Check whether one \
                 step of the cycle is wrong before continuing.",
                steps.len()
            ));
        }
        if repeats >= REPEAT_NOTICE {
            return Verdict::Note(format!(
                "This is the same action for the {repeats}th time. If it is not working, the \
                 screen may have changed — read it again before repeating."
            ));
        }
        if streak >= NO_EFFECT_NOTICE {
            return Verdict::Note(format!(
                "The last {streak} actions had no visible effect. Check the screen before \
                 trying again."
            ));
        }
        Verdict::Clear
    }
}

/// How many trailing steps changed nothing.
///
/// Two screens that are the same picture count as one streak even if the
/// actions differed: the fact being counted is "the screen is stuck", not "this
/// particular action failed".
fn trailing_no_effect(steps: &[&Step]) -> usize {
    steps
        .iter()
        .rev()
        .take_while(|step| step.outcome.is_no_effect())
        .count()
}

/// The shortest period (2 or 3) the trailing steps repeat with, when that is a
/// circle rather than a coincidence.
///
/// Period 1 is not a circle — it is a repeat, which `repeats` already counts.
/// Two repeats of the cycle is the minimum that is evidence of anything.
fn circle_period(steps: &[&Step]) -> Option<usize> {
    for period in [2usize, 3] {
        let needed = period * 2;
        if steps.len() < needed {
            continue;
        }
        let tail = &steps[steps.len() - needed..];
        let repeats = tail
            .iter()
            .skip(period)
            .zip(tail.iter())
            .all(|(later, earlier)| later.signature == earlier.signature);
        if repeats {
            return Some(period);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard::default()
    }

    #[test]
    fn the_first_action_of_a_target_is_clear() {
        let guard = guard();
        assert_eq!(guard.check("conv:1", "tap:send"), Verdict::Clear);
    }

    #[test]
    fn a_repeated_tap_gets_a_note_before_it_gets_a_stop() {
        let guard = guard();
        for i in 0..REPEAT_NOTICE {
            assert!(!guard.check("c", "tap").is_refusal(), "rung {i} refused too early");
            guard.record("c", "tap", Outcome::Changed);
        }
        let verdict = guard.check("c", "tap");
        assert!(matches!(verdict, Verdict::Note(_)), "{verdict:?}");
        assert!(verdict.note().unwrap().contains("same action"));
    }

    #[test]
    fn the_ladder_ends_in_a_refusal() {
        let guard = guard();
        for _ in 0..REPEAT_STOP {
            guard.record("c", "tap", Outcome::Changed);
        }
        let verdict = guard.check("c", "tap");
        assert!(verdict.is_refusal(), "{verdict:?}");
        assert!(verdict.note().is_none(), "a refusal is not also a note");
    }

    #[test]
    fn a_refusal_says_what_to_do_instead() {
        let guard = guard();
        for _ in 0..REPEAT_STOP {
            guard.record("c", "tap", Outcome::Changed);
        }
        let Verdict::Refuse(text) = guard.check("c", "tap") else {
            panic!("expected a refusal");
        };
        assert!(text.contains("android_observe"), "{text}");
        assert!(text.contains("Stop repeating"), "{text}");
    }

    #[test]
    fn different_actions_are_not_the_same_repeat() {
        let guard = guard();
        for i in 0..REPEAT_INSIST {
            guard.record("c", &format!("tap:{i}"), Outcome::Changed);
        }
        // Four distinct actions, each once: nothing is being repeated.
        assert_eq!(guard.check("c", "tap:new"), Verdict::Clear);
    }

    #[test]
    fn a_screen_that_never_moves_is_caught_even_when_the_actions_differ() {
        let guard = guard();
        for i in 0..NO_EFFECT_NOTICE {
            guard.record("c", &format!("tap:{i}"), Outcome::Unchanged);
        }
        let verdict = guard.check("c", "tap:other");
        assert!(matches!(verdict, Verdict::Note(_)), "{verdict:?}");
        assert!(verdict.note().unwrap().contains("no visible effect"));
    }

    #[test]
    fn an_unreadable_screen_is_not_counted_as_no_effect() {
        // A tree that cannot be read is not evidence the screen did not move.
        // Counting it would punish exactly the apps that are hardest to drive.
        let guard = guard();
        for i in 0..NO_EFFECT_STOP {
            guard.record("c", &format!("tap:{i}"), Outcome::Unknown);
        }
        assert_eq!(guard.check("c", "tap:new"), Verdict::Clear);
    }

    #[test]
    fn a_change_resets_the_no_effect_streak() {
        let guard = guard();
        for i in 0..NO_EFFECT_INSIST - 1 {
            guard.record("c", &format!("tap:{i}"), Outcome::Unchanged);
        }
        guard.record("c", "tap:ok", Outcome::Changed);
        assert_eq!(guard.check("c", "tap:next"), Verdict::Clear);
    }

    #[test]
    fn two_screens_in_a_loop_are_called_a_circle() {
        let guard = guard();
        for i in 0..CIRCLE_NOTICE {
            let step = if i % 2 == 0 { "tap:open" } else { "key:BACK" };
            guard.record("c", step, Outcome::Changed);
        }
        let verdict = guard.check("c", "tap:open");
        assert!(matches!(verdict, Verdict::Note(_)), "{verdict:?}");
        assert!(verdict.note().unwrap().contains("cycling"), "{verdict:?}");
    }

    #[test]
    fn a_long_enough_circle_is_stopped() {
        let guard = guard();
        for i in 0..CIRCLE_STOP {
            let step = if i % 2 == 0 { "tap:open" } else { "key:BACK" };
            guard.record("c", step, Outcome::Changed);
        }
        let verdict = guard.check("c", "tap:open");
        assert!(verdict.is_refusal(), "{verdict:?}");
        assert!(verdict.note().is_none());
    }

    #[test]
    fn three_screens_in_a_loop_are_also_a_circle() {
        let guard = guard();
        let cycle = ["tap:a", "tap:b", "tap:c"];
        for i in 0..CIRCLE_NOTICE {
            guard.record("c", cycle[i % 3], Outcome::Changed);
        }
        let verdict = guard.check("c", "tap:a");
        assert!(verdict.note().is_some_and(|n| n.contains("cycling")), "{verdict:?}");
    }

    #[test]
    fn a_one_off_repeat_is_not_a_circle() {
        // "a b a" is not a loop; the ladder must not cry wolf on a back step.
        let guard = guard();
        for step in ["tap:a", "key:BACK", "tap:a"] {
            guard.record("c", step, Outcome::Changed);
        }
        assert_eq!(guard.check("c", "tap:b"), Verdict::Clear);
    }

    #[test]
    fn two_targets_do_not_count_each_other() {
        let guard = guard();
        for _ in 0..REPEAT_STOP {
            guard.record("phone-a:1", "tap", Outcome::Changed);
        }
        assert!(guard.check("phone-a:1", "tap").is_refusal());
        assert_eq!(guard.check("phone-b:2", "tap"), Verdict::Clear);
    }

    #[test]
    fn forgetting_a_target_starts_it_over() {
        let guard = guard();
        for _ in 0..REPEAT_STOP {
            guard.record("c", "tap", Outcome::Changed);
        }
        guard.forget("c");
        assert_eq!(guard.check("c", "tap"), Verdict::Clear);
    }

    #[test]
    fn the_history_is_bounded() {
        let guard = guard();
        for i in 0..(MAX_STEPS * 3) {
            guard.record("c", &format!("tap:{i}"), Outcome::Changed);
        }
        let buckets = guard.lock();
        assert_eq!(buckets["c"].steps.len(), MAX_STEPS);
    }
}
