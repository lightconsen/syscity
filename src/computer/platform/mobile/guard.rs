//! The ladder in front of an action that is being repeated.
//!
//! An agent on a phone fails in more than one shape, and they are not the same
//! shape:
//!
//! - **The same action, over and over.** A tap that worked once is tried again
//!   because the model misread the result, and again, and again.
//! - **No effect.** The action differs each time but the screen does not move:
//!   a button that is disabled, a field that will not focus, a scroll that has
//!   hit the bottom.
//! - **Going in circles.** Two or three screens cycling: open the sheet, close
//!   the sheet, open the sheet.
//! - **Coming back to the same place.** Not a short cycle — the model wanders
//!   and returns to a screen it has already been on, more and more often.
//! - **Reading the same thing again.** The screen has not changed and neither
//!   has the answer, but the model asks again anyway.
//! - **Failing repeatedly.** A tool call that errors is retried unchanged.
//!
//! Mika answers all of these with one `GuardRules` ladder — graded nudges, then
//! a stop, with the model told *which* rung it is on. This is that ladder, with
//! the same numbers and the same ordering: a note the model can still act on
//! before a refusal it cannot argue with.
//!
//! | Signal | Note | Stop |
//! |---|---|---|
//! | the same action again | 3, 6 | 12 |
//! | the same read tool again, whatever it answered | 6, 9 | 15 |
//! | actions in a row that changed nothing | 3, 6, 9 | 12 |
//! | the same unchanged screen read again | 5, 8 | 12 |
//! | consecutive failures | 2, 4 | 8 |
//! | a failure that cannot be retried | 1 | 3 |
//! | back to a state already seen since the last new one | 3, 6 | budget/4, min 10 |
//! | rounds through two or three states | 2, 4 | budget/5, min 6 |
//! | actions in a row that reached no new state | 8, 16 | budget/2, min 24 |
//!
//! The budget is the task's own step count where the caller knows it and
//! [`DEFAULT_BUDGET`] where it does not — a long task is allowed to wander
//! proportionally longer before the safety net closes.
//!
//! # Frames are opt-in
//!
//! The three ladders that count screens (same-state, cycle, no-new-state) need
//! a [`record_with`](Guard::record_with) frame. A caller that never supplies
//! one — [`record`](Guard::record) alone, as the first wiring did — is judged
//! only by the ladders that need no screen, because "reached no new state" is
//! not something that can be observed without a state to compare. Counting
//! blind actions as progress-less would stop a long task that is working fine.
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

use std::collections::{HashMap, HashSet, VecDeque};
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

// ── The codes ──────────────────────────────────────────────────────────────

/// The same action, or the same read, over and over.
pub const REPEATING: &str = "repeating_action";
/// Actions in a row that changed nothing on the screen.
pub const NO_PROGRESS: &str = "no_progress";
/// Back to a screen already visited since the last new one.
pub const SAME_STATE: &str = "same_state";
/// Cycling through two or three screens.
pub const STATE_CYCLE: &str = "state_cycle";
/// Actions in a row that reached no screen this task has not already seen.
pub const NO_NEW_STATE: &str = "no_new_state";
/// The same unchanged screen read again, and again.
pub const READ_LOOP: &str = "read_loop";
/// Consecutive tool calls that failed.
pub const TOO_MANY_FAILURES: &str = "too_many_failures";

/// What the caller should do with the action it is about to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to say.
    Clear,
    /// Go ahead, but tell the model this is rung N of the ladder.
    Note { code: &'static str, text: String },
    /// Do not take it. The message says what to do instead.
    Refuse { code: &'static str, text: String },
}

impl Verdict {
    /// The note to attach to a result, if any.
    pub fn note(&self) -> Option<&str> {
        match self {
            Self::Note { text, .. } => Some(text),
            _ => None,
        }
    }

    /// The reason to return instead of a result, if this is a refusal.
    pub fn refusal(&self) -> Option<&str> {
        match self {
            Self::Refuse { text, .. } => Some(text),
            _ => None,
        }
    }

    /// Which ladder spoke, for a caller that wants to branch on it.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Note { code, .. } | Self::Refuse { code, .. } => Some(code),
            Self::Clear => None,
        }
    }

    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Refuse { .. })
    }

    fn warn(code: &'static str, text: String) -> Self {
        Self::Note { code, text }
    }

    fn halt(code: &'static str, text: String) -> Self {
        Self::Refuse { code, text }
    }
}

/// What the caller knows about the task this action belongs to.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scope {
    /// The task's step budget, when the caller knows it. The budget-scaled
    /// ladders use [`DEFAULT_BUDGET`] when this is `None`.
    pub budget: Option<usize>,
}

/// One action this conversation took on one target.
#[derive(Debug, Clone)]
struct Step {
    signature: String,
    outcome: Outcome,
    /// The screen it left behind, when the caller could name one.
    frame: Option<u64>,
    at: Instant,
}

/// A conversation's history for one target.
#[derive(Debug, Default)]
struct Bucket {
    steps: VecDeque<Step>,
    last_touch: Option<Instant>,

    /// Whether any step has ever carried a frame. Until one does, the
    /// screen-counting ladders have nothing to count and stay silent.
    frames_seen: bool,

    /// Consecutive actions whose screen did not change.
    unchanged: usize,
    unchanged_warned_at: usize,

    /// Re-reads of a screen that has not changed since it last did.
    rereads: usize,
    reread_warned_at: usize,
    /// What the last read returned, so a re-read can be told from a new answer.
    last_read: Option<u64>,
    /// The read tool called last, and how many times in a row.
    read_tool: Option<String>,
    read_tool_repeats: usize,
    read_tool_warned_at: usize,

    /// Consecutive failed tool calls, and how many cannot be retried.
    failures: usize,
    unrecoverable: usize,
    /// 0 = nothing said yet, 1 = the warning rung was delivered.
    failure_warned_at: u8,

    /// How often each screen has been reached since the last new one.
    visits: HashMap<u64, usize>,
    visit_warned_at: HashMap<u64, usize>,
    /// Every screen this task has been on, for "is this new?".
    seen: HashSet<u64>,
    /// The screens in the order they were reached, adjacent repeats dropped.
    trail: VecDeque<u64>,
    cycle_warned_at: usize,
    since_new_state: usize,
    since_new_warned_at: usize,

    repeat_warned_at: usize,
}

impl Bucket {
    /// A new screen: what was counted against "this is going nowhere" is done.
    fn progress(&mut self) {
        self.unchanged = 0;
        self.unchanged_warned_at = 0;
        self.visits.clear();
        self.visit_warned_at.clear();
        self.since_new_state = 0;
        self.since_new_warned_at = 0;
    }

    /// The screen changed: what a read of the old one said is history.
    fn page_changed(&mut self) {
        self.rereads = 0;
        self.reread_warned_at = 0;
        self.last_read = None;
        self.read_tool = None;
        self.read_tool_repeats = 0;
        self.read_tool_warned_at = 0;
    }

    /// The screen the task is on now, if the last action left one.
    fn current(&self) -> Option<u64> {
        self.trail.back().copied()
    }
}

/// How long a quiet target is remembered.
const WINDOW: Duration = Duration::from_secs(20 * 60);

/// The most steps one bucket keeps, whatever the window says.
///
/// The ladder's top rung is a couple of dozen; keeping more than that would
/// only measure a loop that has already been stopped.
const MAX_STEPS: usize = 32;

/// How many screens the trail keeps, for the cycle detector.
const TRAIL_KEEP: usize = 64;

/// The task length a budget-scaled ladder assumes when the caller does not
/// know one. Mika's number, so the two implementations grade a task the same.
pub const DEFAULT_BUDGET: usize = 60;

/// Repeat thresholds: note, insist, stop.
const REPEAT: [usize; 3] = [3, 6, 12];

/// Repeats of the *same read tool*, whatever it answered. Higher than an
/// action's ladder because reading a screen twice is how a careful reader
/// works, not a symptom.
const REPEAT_DIAGNOSTIC: [usize; 3] = [6, 9, 15];

/// Consecutive no-effect thresholds: note, note, insist, stop.
const NO_EFFECT: [usize; 4] = [3, 6, 9, 12];

/// Re-reads of a screen that has not changed: note, insist, stop.
const REREAD: [usize; 3] = [5, 8, 12];

/// Consecutive failures: note, insist, stop.
const FAILURE: [usize; 3] = [2, 4, 8];

/// From this failure on, the note says what to do about it.
const FAILURE_WAYS_OUT: usize = 4;

/// Failures that cannot be retried at all: note, stop.
const UNRECOVERABLE: [usize; 2] = [1, 3];

/// The stop rung of a budget-scaled ladder is `budget / divisor`, never below
/// `floor`: a safety net has to sit well past the notes, however short the task.
fn late_stop(budget: Option<usize>, divisor: usize, floor: usize) -> usize {
    floor.max(budget.unwrap_or(DEFAULT_BUDGET) / divisor)
}

/// Returning to a screen already seen: note, insist, stop.
fn same_state_ladder(budget: Option<usize>) -> [usize; 3] {
    [3, 6, late_stop(budget, 4, 10)]
}

/// Rounds through two or three screens: note, insist, stop.
fn cycle_ladder(budget: Option<usize>) -> [usize; 3] {
    [2, 4, late_stop(budget, 5, 6)]
}

/// Actions in a row that reached no new screen: note, insist, stop. Long on
/// purpose — a task can legitimately spend many steps on one screen.
fn no_new_state_ladder(budget: Option<usize>) -> [usize; 3] {
    [8, 16, late_stop(budget, 2, 24)]
}

/// One rung of a ladder.
#[derive(Debug, Clone, Copy)]
struct Rung {
    threshold: usize,
    stop: bool,
}

/// The highest rung `count` has reached, when it has not been delivered yet.
///
/// Suppressing an already-delivered note is what keeps the ladder from nagging:
/// the model was told at three, and telling it again at four would spend its
/// one line of attention on something it already knows. A stop is never
/// suppressed — the action it refuses would otherwise go through on the second
/// attempt, which is exactly the silence the guard exists to break.
fn climb(count: usize, ladder: &[usize], warned_at: usize, can_stop: bool) -> Option<Rung> {
    let threshold = *ladder.iter().rev().find(|&&threshold| count >= threshold)?;
    let stop = can_stop && threshold == ladder[ladder.len() - 1];
    if !stop && threshold <= warned_at {
        return None;
    }
    Some(Rung { threshold, stop })
}

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
        self.check_in(key, signature, Scope::default())
    }

    /// [`check`] with the task's step budget.
    pub fn check_in(&self, key: &str, signature: &str, scope: Scope) -> Verdict {
        let mut buckets = self.lock();
        let now = Instant::now();
        let bucket = buckets.entry(key.to_string()).or_default();
        Self::prune(bucket, now);
        Self::judge(bucket, signature, scope)
    }

    /// Record that `signature` ran and what the screen did.
    ///
    /// No frame: the screen-counting ladders stay silent for this bucket until
    /// a caller supplies one through [`record_with`](Self::record_with).
    pub fn record(&self, key: &str, signature: &str, outcome: Outcome) {
        self.record_with(key, signature, outcome, None);
    }

    /// [`record`] with the screen the action left behind.
    ///
    /// The frame is what turns "something happened" into "the same screen
    /// again". Two actions with different signatures that leave the same frame
    /// are one stuck screen, and three screens cycling are one loop; without it
    /// neither can be seen.
    pub fn record_with(&self, key: &str, signature: &str, outcome: Outcome, frame: Option<u64>) {
        let mut buckets = self.lock();
        let now = Instant::now();
        let bucket = buckets.entry(key.to_string()).or_default();
        Self::prune(bucket, now);
        bucket.last_touch = Some(now);

        if outcome.is_no_effect() {
            bucket.unchanged += 1;
        } else {
            bucket.unchanged = 0;
            bucket.unchanged_warned_at = 0;
        }

        let previous = bucket.steps.back().and_then(|step| step.frame);
        bucket.steps.push_back(Step {
            signature: signature.to_string(),
            outcome,
            frame,
            at: now,
        });
        while bucket.steps.len() > MAX_STEPS {
            bucket.steps.pop_front();
        }

        let Some(frame) = frame else {
            return;
        };
        bucket.frames_seen = true;

        // The same screen again: the action reached nothing new, but it is not
        // a *return* either — it never left.
        if previous == Some(frame) {
            bucket.since_new_state += 1;
            return;
        }

        // A different screen. What the last read said about the old one is
        // history, and a screen changed under a reader is not a re-read.
        bucket.page_changed();
        bucket.trail.push_back(frame);
        while bucket.trail.len() > TRAIL_KEEP {
            bucket.trail.pop_front();
        }

        if bucket.seen.insert(frame) {
            // A screen this task has never been on. Everything counted against
            // "this is going nowhere" is done.
            bucket.progress();
            bucket.visits.insert(frame, 1);
        } else {
            *bucket.visits.entry(frame).or_insert(0) += 1;
            bucket.since_new_state += 1;
        }

        // A cycle that has broken is not the cycle being counted.
        if circle_rounds(&bucket.trail).is_none() {
            bucket.cycle_warned_at = 0;
        }
    }

    /// Whether a read of `tool` should be refused before it happens.
    ///
    /// Symmetric with [`check`](Self::check): asked before the read, so a
    /// refusal saves the round it would have spent. The content is not known
    /// yet, so only the *repeat of the read tool* can be judged here; the
    /// re-read ladder needs the answer and is handled by
    /// [`record_read`](Self::record_read).
    pub fn check_read(&self, key: &str, tool: &str) -> Verdict {
        let mut buckets = self.lock();
        let bucket = buckets.entry(key.to_string()).or_default();

        if let Some(rung) = climb(bucket.rereads, &REREAD, bucket.reread_warned_at, true) {
            if rung.stop {
                return Verdict::halt(
                    READ_LOOP,
                    format!(
                        "Refused: this screen has been read {} times and it has not changed, so \
                         reading it again cannot tell you anything new. Act on what you have — \
                         or, if the screen is not responding, stop and report what is blocking \
                         the task.",
                        bucket.rereads
                    ),
                );
            }
        }
        if let Some(rung) =
            climb(bucket.read_tool_repeats, &REPEAT_DIAGNOSTIC, bucket.read_tool_warned_at, true)
        {
            if rung.stop {
                return Verdict::halt(
                    REPEATING,
                    format!(
                        "Refused: `{tool}` has been called {} times in a row. Looking at the \
                         phone again is not going to move this along — act on what you already \
                         know, or stop and report what is blocking the task.",
                        bucket.read_tool_repeats
                    ),
                );
            }
        }
        Verdict::Clear
    }

    /// Record a read of the screen, and return the note to attach to it.
    ///
    /// `content` is what the read answered — a tree fingerprint, a page hash.
    /// The same answer twice means the screen did not change between them, and
    /// asking a third time cannot produce a different one.
    pub fn record_read(&self, key: &str, tool: &str, content: Option<u64>) -> Option<String> {
        let mut buckets = self.lock();
        let now = Instant::now();
        let bucket = buckets.entry(key.to_string()).or_default();
        Self::prune(bucket, now);
        bucket.last_touch = Some(now);

        // The same tool again, whatever it answered. Reading a screen twice is
        // how a careful reader works, so this ladder sits higher than an
        // action's.
        if bucket.read_tool.as_deref() == Some(tool) {
            bucket.read_tool_repeats += 1;
        } else {
            bucket.read_tool = Some(tool.to_string());
            bucket.read_tool_repeats = 1;
            bucket.read_tool_warned_at = 0;
        }

        // The same screen again. Unknown content is not evidence of a re-read,
        // for the same reason an unreadable screen is not evidence of no
        // effect: it is the absence of an answer, not an answer.
        let fresh = match (content, bucket.last_read) {
            (Some(now), Some(before)) => now != before,
            _ => true,
        };
        if fresh {
            bucket.last_read = content;
            bucket.rereads = 0;
            bucket.reread_warned_at = 0;
        } else {
            bucket.rereads += 1;
        }

        if let Some(rung) = climb(bucket.rereads, &REREAD, bucket.reread_warned_at, true) {
            if rung.stop {
                return None;
            }
            bucket.reread_warned_at = rung.threshold;
            return Some(format!(
                "This is read {} of the same unchanged screen. If it did not answer your \
                 question the first time it will not answer it now — act on what is there, or \
                 read something else.",
                bucket.rereads
            ));
        }
        if let Some(rung) =
            climb(bucket.read_tool_repeats, &REPEAT_DIAGNOSTIC, bucket.read_tool_warned_at, true)
        {
            if rung.stop {
                return None;
            }
            bucket.read_tool_warned_at = rung.threshold;
            return Some(format!(
                "`{tool}` has been called {} times in a row. Each read is a turn spent, so act \
                 on what you have unless this one tells you something the last one did not.",
                bucket.read_tool_repeats
            ));
        }
        None
    }

    /// Record that a tool call failed.
    ///
    /// `recoverable` is whether trying the same thing again could work. A call
    /// that failed because the target vanished is recoverable — the model may
    /// have misread the screen. One that failed because the phone has no such
    /// feature is not, and three of those is a task that cannot be done this
    /// way.
    pub fn record_failure(&self, key: &str, recoverable: bool) {
        let mut buckets = self.lock();
        let bucket = buckets.entry(key.to_string()).or_default();
        bucket.failures += 1;
        if !recoverable {
            bucket.unrecoverable += 1;
        }
    }

    /// Record that a tool call succeeded, which ends the failure streak.
    pub fn record_success(&self, key: &str) {
        let mut buckets = self.lock();
        let bucket = buckets.entry(key.to_string()).or_default();
        bucket.failures = 0;
        bucket.unrecoverable = 0;
        bucket.failure_warned_at = 0;
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
    /// Strongest first, so a stop is never reported as a note, and a stop is
    /// reported before any note even when a lower ladder fired sooner.
    fn judge(bucket: &mut Bucket, signature: &str, scope: Scope) -> Verdict {
        // A call that failed is worth saying before anything about what the
        // screen did — the screen may not even have been read.
        match failure_severity(bucket) {
            Severity::Stop => {
                return Verdict::halt(
                    TOO_MANY_FAILURES,
                    failed_message(bucket.failures, bucket.unrecoverable),
                )
            }
            Severity::Warn if bucket.failure_warned_at < warn_rung(bucket) => {
                bucket.failure_warned_at = warn_rung(bucket);
                return Verdict::warn(
                    TOO_MANY_FAILURES,
                    failed_note(bucket.failures, bucket.unrecoverable),
                );
            }
            _ => {}
        }

        let repeats = bucket
            .steps
            .iter()
            .filter(|step| step.signature == signature)
            .count();
        if let Some(rung) = climb(repeats, &REPEAT, bucket.repeat_warned_at, true) {
            if rung.stop {
                return Verdict::halt(
                    REPEATING,
                    format!(
                        "Refused: this is the same action for the {repeats}th time and it is not \
                         getting anywhere. Stop repeating it — read the screen with \
                         android_observe, work out why it is not doing what you expect, and do \
                         something else. If the task cannot be finished, say what is blocking it."
                    ),
                );
            }
            bucket.repeat_warned_at = rung.threshold;
            return Verdict::warn(
                REPEATING,
                if rung.threshold == REPEAT[1] {
                    format!(
                        "You have done this same action {repeats} times. It is not producing \
                         progress. Read the screen before doing it again."
                    )
                } else {
                    format!(
                        "This is the same action for the {repeats}th time. If it is not working, \
                         the screen may have changed — read it again before repeating."
                    )
                },
            );
        }

        let streak = trailing_no_effect(&bucket.steps);
        if let Some(rung) = climb(streak, &NO_EFFECT, bucket.unchanged_warned_at, true) {
            if rung.stop {
                return Verdict::halt(
                    NO_PROGRESS,
                    format!(
                        "Refused: {streak} actions in a row changed nothing on the screen. \
                         Repeating this will not change that. Read the screen with \
                         android_observe, and if it still does not respond, stop and report what \
                         is blocking the task."
                    ),
                );
            }
            bucket.unchanged_warned_at = rung.threshold;
            return Verdict::warn(
                NO_PROGRESS,
                if rung.threshold >= NO_EFFECT[2] {
                    format!(
                        "The last {streak} actions changed nothing on the screen. Whatever you \
                         are attempting is not reaching it — read the screen with android_observe \
                         and reconsider the target rather than retrying."
                    )
                } else {
                    format!(
                        "The last {streak} actions had no visible effect. Check the screen \
                         before trying again."
                    )
                },
            );
        }

        // The screen-counting ladders need a screen to count. A bucket that has
        // never been given a frame is judged only by the ladders above.
        if !bucket.frames_seen {
            return Verdict::Clear;
        }

        if let Some(rung) = climb(bucket.rereads, &REREAD, usize::MAX, true) {
            // Notes were delivered by `record_read`; only the stop matters
            // here, and a stop is never suppressed.
            if rung.stop {
                return Verdict::halt(
                    READ_LOOP,
                    format!(
                        "Refused: this screen has been read {} times without changing. Reading \
                         it again will not help. Act on what you have, or stop and report what \
                         is blocking the task.",
                        bucket.rereads
                    ),
                );
            }
        }

        let rounds = circle_rounds(&bucket.trail).unwrap_or(0);
        if let Some(rung) = climb(rounds, &cycle_ladder(scope.budget), bucket.cycle_warned_at, true)
        {
            let states = circle_period(&bucket.trail).unwrap_or(2);
            if rung.stop {
                return Verdict::halt(
                    STATE_CYCLE,
                    format!(
                        "Refused: the last actions are cycling through {states} screens without \
                         progress. This is a loop. Read the screen with android_observe, decide \
                         which step of the cycle is the wrong one, and break it — or stop and \
                         report what is blocking the task."
                    ),
                );
            }
            bucket.cycle_warned_at = rung.threshold;
            return Verdict::warn(
                STATE_CYCLE,
                format!(
                    "The last actions are cycling through {states} screens ({rounds} full \
                     rounds). Check whether one step of the cycle is wrong before continuing."
                ),
            );
        }

        let returns = bucket
            .current()
            .and_then(|frame| bucket.visits.get(&frame).copied())
            .unwrap_or(0);
        if returns >= 2 {
            let warned_at = bucket
                .current()
                .and_then(|frame| bucket.visit_warned_at.get(&frame).copied())
                .unwrap_or(0);
            if let Some(rung) = climb(returns, &same_state_ladder(scope.budget), warned_at, true) {
                if let Some(frame) = bucket.current() {
                    bucket.visit_warned_at.insert(frame, rung.threshold);
                }
                if rung.stop {
                    return Verdict::halt(
                        SAME_STATE,
                        format!(
                            "Refused: this screen has been reached {returns} times since anything \
                             new happened. Coming back to it again will not move the task on. \
                             Read it with android_observe, and if the way forward is not there, \
                             stop and report what is blocking the task."
                        ),
                    );
                }
                return Verdict::warn(
                    SAME_STATE,
                    format!(
                        "This is the {returns}th time back on this screen without anything new \
                         happening. Something in this cycle is probably wrong."
                    ),
                );
            }
        }

        if let Some(rung) = climb(
            bucket.since_new_state,
            &no_new_state_ladder(scope.budget),
            bucket.since_new_warned_at,
            true,
        ) {
            if rung.stop {
                return Verdict::halt(
                    NO_NEW_STATE,
                    format!(
                        "Refused: {} actions in a row have not reached a screen this task has \
                         not already seen. The current approach is not working. Stop, and report \
                         what is blocking the task.",
                        bucket.since_new_state
                    ),
                );
            }
            bucket.since_new_warned_at = rung.threshold;
            return Verdict::warn(
                NO_NEW_STATE,
                format!(
                    "{} actions in a row have not reached anything new. Consider that this is \
                     the wrong route and check the screen before continuing.",
                    bucket.since_new_state
                ),
            );
        }

        Verdict::Clear
    }
}

/// How hard the failure ladder should speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Silent,
    Warn,
    Stop,
}

/// Which warning rung a run of failures has reached: 0 none, 1 the first note,
/// 2 the note that also says what else to try.
///
/// The second note exists because the first one's advice — "work out why" — is
/// exhausted by the fourth failure, and repeating the same sentence would be
/// the nagging the once-per-rung rule avoids while saying nothing new. So the
/// ladder has one note that changes content, at the point it changes.
fn warn_rung(bucket: &Bucket) -> u8 {
    if bucket.failures >= FAILURE_WAYS_OUT || bucket.unrecoverable > 0 {
        2
    } else if bucket.failures >= FAILURE[0] || bucket.unrecoverable >= UNRECOVERABLE[0] {
        1
    } else {
        0
    }
}

/// Mika's `failures()`: the severity a run of failures has earned.
///
/// Stop if either ladder is at its top rung; warn if either is at its first.
/// The unrecoverable rung is reached on the *first* failure of that kind,
/// because there is no retry to spend discovering it.
fn failure_severity(bucket: &Bucket) -> Severity {
    if bucket.failures >= FAILURE[2] || bucket.unrecoverable >= UNRECOVERABLE[1] {
        Severity::Stop
    } else if bucket.failures >= FAILURE[0] || bucket.unrecoverable >= UNRECOVERABLE[0] {
        Severity::Warn
    } else {
        Severity::Silent
    }
}

/// What to do about a run of failures.
fn failed_message(failures: usize, unrecoverable: usize) -> String {
    if unrecoverable >= UNRECOVERABLE[1] {
        return format!(
            "Refused: {unrecoverable} of the last {failures} calls cannot be retried — the phone \
             does not support what was asked, or the app is missing. Retrying will fail the same \
             way. Stop and report what is blocking the task, or choose a different approach."
        );
    }
    format!(
        "Refused: {failures} tool calls in a row failed. The next one will fail the same way \
         unless something changes — read the screen with android_observe and find out what is \
         actually there before trying again.{}",
        ways_out(failures, unrecoverable)
    )
}

/// The note attached before a run of failures becomes a refusal.
fn failed_note(failures: usize, unrecoverable: usize) -> String {
    format!(
        "The last {failures} calls failed. Before trying again, work out why — the screen may \
         not be what you think it is.{}",
        ways_out(failures, unrecoverable)
    )
}

/// "What else you could do", once the run is long enough to need it.
fn ways_out(failures: usize, unrecoverable: usize) -> String {
    if failures >= FAILURE_WAYS_OUT || unrecoverable > 0 {
        " Try a visibly different route, ask what to do, or hand back what you already have and \
         say what is missing."
            .to_string()
    } else {
        String::new()
    }
}

/// How many trailing steps changed nothing.
///
/// Two screens that are the same picture count as one streak even if the
/// actions differed: the fact being counted is "the screen is stuck", not "this
/// particular action failed".
fn trailing_no_effect(steps: &VecDeque<Step>) -> usize {
    steps
        .iter()
        .rev()
        .take_while(|step| step.outcome.is_no_effect())
        .count()
}

/// The shortest period (2 or 3) the trail repeats with, when that is a circle
/// rather than a coincidence.
///
/// Period 1 is not a circle — it is a repeat, which the repeat ladder already
/// counts. Two repeats of the cycle is the minimum that is evidence of
/// anything.
fn circle_period(trail: &VecDeque<u64>) -> Option<usize> {
    for period in [2usize, 3] {
        let needed = period * 2;
        if trail.len() < needed {
            continue;
        }
        // The last `period` screens must be distinct, or this is not a cycle
        // through that many screens.
        let tail: Vec<u64> = trail.iter().copied().skip(trail.len() - needed).collect();
        let last: HashSet<u64> = tail[needed - period..].iter().copied().collect();
        if last.len() != period {
            continue;
        }
        let repeats = tail
            .iter()
            .skip(period)
            .zip(tail.iter())
            .all(|(later, earlier)| later == earlier);
        if repeats {
            return Some(period);
        }
    }
    None
}

/// How many whole rounds the trail has been going in circles for.
///
/// Mika counts rounds, not steps: "two rounds through two screens" is the
/// signal, and a four-screen cycle must not read as two full rounds. The count
/// includes every step that is part of the repeating tail, divided by the
/// period.
fn circle_rounds(trail: &VecDeque<u64>) -> Option<usize> {
    for period in [2usize, 3] {
        if trail.len() < period + 1 {
            continue;
        }
        // The last `period` screens are the cycle; if they repeat among
        // themselves it is a shorter cycle, counted under its own period.
        let last: HashSet<u64> = trail.iter().copied().skip(trail.len() - period).collect();
        if last.len() != period {
            continue;
        }
        let mut length = period;
        let mut index = trail.len();
        // Extend backwards while each screen matches the one a period before.
        while index > period {
            let later = trail[index - 1];
            let earlier = trail[index - 1 - period];
            if later != earlier {
                break;
            }
            length += 1;
            index -= 1;
        }
        if length > period {
            return Some(length / period);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh bucket key per test call, so the process-wide guard's buckets
    /// do not leak between tests.
    fn key() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Guard::bucket_key(&format!("t-{}", NEXT.fetch_add(1, Ordering::Relaxed)), "display")
    }

    // ── the same action ────────────────────────────────────────────────────

    #[test]
    fn the_first_two_repeats_are_silent() {
        let guard = Guard::default();
        let key = key();
        guard.record(&key, "tap:ok", Outcome::Changed);
        assert_eq!(guard.check(&key, "tap:ok"), Verdict::Clear);
        guard.record(&key, "tap:ok", Outcome::Changed);
        assert_eq!(guard.check(&key, "tap:ok"), Verdict::Clear);
        guard.record(&key, "tap:ok", Outcome::Changed);
        assert_eq!(guard.check(&key, "tap:ok").code(), Some(REPEATING));
    }

    #[test]
    fn the_third_repeat_is_a_note() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..3 {
            guard.record(&key, "tap:ok", Outcome::Changed);
        }
        let verdict = guard.check(&key, "tap:ok");
        assert_eq!(verdict.code(), Some(REPEATING));
        assert!(!verdict.is_refusal());
    }

    #[test]
    fn a_note_is_not_repeated_on_the_next_step() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..3 {
            guard.record(&key, "tap:ok", Outcome::Changed);
        }
        assert!(guard.check(&key, "tap:ok").note().is_some());
        guard.record(&key, "tap:ok", Outcome::Changed);
        assert_eq!(guard.check(&key, "tap:ok"), Verdict::Clear);
    }

    #[test]
    fn the_sixth_repeat_is_a_stronger_note() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..3 {
            guard.record(&key, "tap:ok", Outcome::Changed);
        }
        assert!(guard.check(&key, "tap:ok").note().is_some());
        for _ in 0..3 {
            guard.record(&key, "tap:ok", Outcome::Changed);
        }
        let verdict = guard.check(&key, "tap:ok");
        assert!(verdict.note().unwrap().contains("not producing progress"));
    }

    #[test]
    fn the_twelfth_repeat_is_refused_and_stays_refused() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..12 {
            guard.record(&key, "tap:ok", Outcome::Changed);
        }
        assert!(guard.check(&key, "tap:ok").is_refusal());
        // A stop is not a one-shot: the action would go through otherwise.
        assert!(guard.check(&key, "tap:ok").is_refusal());
    }

    #[test]
    fn a_different_action_does_not_raise_the_repeat_ladder() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..20 {
            guard.record(&key, "tap:a", Outcome::Changed);
            guard.record(&key, "tap:b", Outcome::Changed);
        }
        // Each signature is at 20: the repeat ladder refuses either one. The
        // point of the test is that no *other* signature is refused.
        assert!(!guard.check(&key, "tap:c").is_refusal());
    }

    // ── no effect ──────────────────────────────────────────────────────────

    #[test]
    fn two_dead_taps_are_silent_and_the_third_is_a_note() {
        let guard = Guard::default();
        let key = key();
        guard.record(&key, "tap:x", Outcome::Unchanged);
        assert_eq!(guard.check(&key, "tap:y"), Verdict::Clear);
        guard.record(&key, "tap:x", Outcome::Unchanged);
        assert_eq!(guard.check(&key, "tap:y"), Verdict::Clear);
        guard.record(&key, "tap:x", Outcome::Unchanged);
        let verdict = guard.check(&key, "tap:y");
        assert_eq!(verdict.code(), Some(NO_PROGRESS));
        assert!(!verdict.is_refusal());
    }

    #[test]
    fn an_unreadable_screen_is_not_evidence_of_no_effect() {
        let guard = Guard::default();
        let key = key();
        for index in 0..20 {
            guard.record(&key, &format!("tap:{index}"), Outcome::Unknown);
        }
        assert_eq!(guard.check(&key, "tap:next"), Verdict::Clear);
    }

    #[test]
    fn the_twelfth_dead_action_is_refused() {
        let guard = Guard::default();
        let key = key();
        for index in 0..12 {
            guard.record(&key, &format!("tap:{index}"), Outcome::Unchanged);
        }
        assert!(guard.check(&key, "tap:next").is_refusal());
    }

    #[test]
    fn one_that_worked_breaks_the_dead_streak() {
        let guard = Guard::default();
        let key = key();
        for index in 0..11 {
            guard.record(&key, &format!("tap:{index}"), Outcome::Unchanged);
        }
        guard.record(&key, "tap:y", Outcome::Changed);
        assert_eq!(guard.check(&key, "tap:z").code(), None);
    }

    // ── reads ──────────────────────────────────────────────────────────────

    #[test]
    fn reading_a_changing_screen_never_claims_it_is_stuck() {
        let guard = Guard::default();
        let key = key();
        // Stay under the tool-repeat stop: this is about the *screen* ladders.
        for frame in 0..14 {
            assert!(!guard.check_read(&key, "android_ui_tree").is_refusal());
            let note = guard.record_read(&key, "android_ui_tree", Some(frame));
            // Nothing may claim the screen is unchanged when it is not.
            assert!(note
                .as_deref()
                .is_none_or(|note| !note.contains("unchanged")));
        }
    }

    #[test]
    fn rereading_the_same_screen_warns_after_five() {
        let guard = Guard::default();
        let key = key();
        // The first read is not a re-read; the fifth re-read is the sixth call.
        let mut notes = Vec::new();
        for _ in 0..6 {
            notes.push(guard.record_read(&key, "android_ui_tree", Some(7)));
        }
        assert!(notes[..5].iter().all(|note| note.is_none()));
        assert!(notes[5].as_deref().unwrap().contains("unchanged screen"));
    }

    #[test]
    fn rereading_the_same_screen_thirteen_times_is_refused() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..13 {
            guard.record_read(&key, "android_ui_tree", Some(7));
        }
        let verdict = guard.check_read(&key, "android_ui_tree");
        assert!(verdict.is_refusal());
        assert_eq!(verdict.code(), Some(READ_LOOP));
    }

    #[test]
    fn a_new_answer_resets_the_reread_count() {
        let guard = Guard::default();
        let key = key();
        // Twelve identical reads reach the re-read stop…
        for _ in 0..13 {
            guard.record_read(&key, "android_ui_tree", Some(1));
        }
        assert!(guard.check_read(&key, "android_ui_tree").is_refusal());
        // …and a screen that then changes starts the re-read run over. The
        // tool-repeat ladder still holds (it counts calls, not answers), so
        // read with a different tool to observe the re-read counter itself.
        guard.record_read(&key, "android_observe", Some(2));
        assert!(!guard.check_read(&key, "android_observe").is_refusal());
        for _ in 0..4 {
            assert_eq!(guard.record_read(&key, "android_observe", Some(2)), None);
        }
    }

    #[test]
    fn the_same_read_tool_six_times_is_a_note_even_while_it_answers() {
        let guard = Guard::default();
        let key = key();
        let mut notes = Vec::new();
        for frame in 0..7 {
            notes.push(guard.record_read(&key, "android_ui_tree", Some(frame)));
        }
        // Every screen is new, so only the tool-repeat ladder can speak.
        assert!(notes[..5].iter().all(|note| note.is_none()));
        assert!(notes[5].as_deref().unwrap().contains("android_ui_tree"));
    }

    #[test]
    fn a_different_read_tool_resets_the_read_repeat() {
        let guard = Guard::default();
        let key = key();
        for frame in 0..6 {
            guard.record_read(&key, "android_ui_tree", Some(frame));
        }
        assert_eq!(guard.record_read(&key, "android_observe", Some(100)), None);
    }

    #[test]
    fn an_unreadable_answer_is_not_a_reread() {
        let guard = Guard::default();
        let key = key();
        let mut notes = Vec::new();
        for _ in 0..20 {
            notes.push(guard.record_read(&key, "android_ui_tree", None));
        }
        // The tool-repeat ladder still counts, but nothing claims the screen
        // was read unchanged.
        assert!(notes
            .iter()
            .flatten()
            .all(|note| !note.contains("unchanged screen")));
    }

    // ── failures ───────────────────────────────────────────────────────────

    #[test]
    fn two_failures_are_a_note() {
        let guard = Guard::default();
        let key = key();
        guard.record_failure(&key, true);
        assert_eq!(guard.check(&key, "tap:x"), Verdict::Clear);
        guard.record_failure(&key, true);
        let verdict = guard.check(&key, "tap:x");
        assert_eq!(verdict.code(), Some(TOO_MANY_FAILURES));
        assert!(!verdict.is_refusal());
    }

    #[test]
    fn the_failure_note_is_delivered_once() {
        let guard = Guard::default();
        let key = key();
        guard.record_failure(&key, true);
        guard.record_failure(&key, true);
        assert!(guard.check(&key, "tap:x").note().is_some());
        assert_eq!(guard.check(&key, "tap:x"), Verdict::Clear);
    }

    #[test]
    fn eight_failures_are_refused() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..8 {
            guard.record_failure(&key, true);
        }
        assert!(guard.check(&key, "tap:x").is_refusal());
    }

    #[test]
    fn four_failures_say_what_else_to_try() {
        let guard = Guard::default();
        let key = key();
        guard.record_failure(&key, true);
        guard.record_failure(&key, true);
        // The first note does not offer a way out…
        assert!(!guard
            .check(&key, "tap:x")
            .note()
            .unwrap()
            .contains("different route"));
        // …and it is not repeated while the advice is still the same one.
        guard.record_failure(&key, true);
        assert_eq!(guard.check(&key, "tap:x"), Verdict::Clear);
        // The fourth failure is a different fact: the "find out why" advice has
        // been tried, so the note changes rather than repeating.
        guard.record_failure(&key, true);
        assert!(guard
            .check(&key, "tap:x")
            .note()
            .unwrap()
            .contains("different route"));
    }

    #[test]
    fn three_failures_that_cannot_be_retried_stop_the_task() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..3 {
            guard.record_failure(&key, false);
        }
        let verdict = guard.check(&key, "tap:x");
        assert!(verdict.is_refusal());
        assert!(verdict.refusal().unwrap().contains("cannot be retried"));
    }

    #[test]
    fn a_success_clears_the_failure_ladder() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..7 {
            guard.record_failure(&key, true);
        }
        assert!(guard.check(&key, "tap:x").note().is_some());
        guard.record_success(&key);
        assert_eq!(guard.check(&key, "tap:x"), Verdict::Clear);
    }

    // ── state history ──────────────────────────────────────────────────────

    #[test]
    fn frames_are_optional_and_blind_actions_never_trip_a_screen_ladder() {
        let guard = Guard::default();
        let key = key();
        // 40 distinct actions with no frame: long, but nothing says the screen
        // is stuck, because nobody ever measured it.
        for index in 0..40 {
            guard.record(&key, &format!("tap:{index}"), Outcome::Changed);
            assert!(!guard.check(&key, &format!("tap:{index}")).is_refusal());
        }
    }

    #[test]
    fn a_new_screen_is_progress() {
        let guard = Guard::default();
        let key = key();
        for frame in 0..20 {
            guard.record_with(&key, &format!("tap:{frame}"), Outcome::Changed, Some(frame));
        }
        assert_eq!(guard.check(&key, "tap:next"), Verdict::Clear);
    }

    #[test]
    fn cycling_two_screens_is_noticed_after_two_rounds() {
        let guard = Guard::default();
        let key = key();
        for index in 0..4 {
            guard.record_with(
                &key,
                &format!("tap:{index}"),
                Outcome::Changed,
                Some((index % 2) as u64),
            );
        }
        let verdict = guard.check(&key, "tap:next");
        assert_eq!(verdict.code(), Some(STATE_CYCLE));
        assert!(verdict.note().unwrap().contains("2 screens"));
    }

    #[test]
    fn cycling_three_screens_is_not_read_as_a_two_screen_cycle() {
        let guard = Guard::default();
        let key = key();
        for index in 0..6 {
            guard.record_with(
                &key,
                &format!("tap:{index}"),
                Outcome::Changed,
                Some((index % 3) as u64),
            );
        }
        let verdict = guard.check(&key, "tap:next");
        assert_eq!(verdict.code(), Some(STATE_CYCLE));
        assert!(verdict.note().unwrap().contains("3 screens"));
    }

    #[test]
    fn a_broken_cycle_stops_being_counted() {
        let guard = Guard::default();
        let key = key();
        for index in 0..8 {
            guard.record_with(
                &key,
                &format!("tap:{index}"),
                Outcome::Changed,
                Some((index % 2) as u64),
            );
        }
        assert_eq!(guard.check(&key, "tap:next").code(), Some(STATE_CYCLE));
        // Four screens this task has never seen: the cycle is over.
        for frame in 100..104 {
            guard.record_with(&key, &format!("tap:{frame}"), Outcome::Changed, Some(frame));
        }
        assert_eq!(guard.check(&key, "tap:next").code(), None);
    }

    #[test]
    fn coming_back_to_a_screen_three_times_is_a_note() {
        let guard = Guard::default();
        let key = key();
        // Four new screens establish the states; then a wander back to the
        // same one three times, with no period, so it is not a cycle.
        for frame in 0..4u64 {
            guard.record_with(&key, &format!("setup:{frame}"), Outcome::Changed, Some(frame));
        }
        for (index, frame) in [0u64, 2, 0, 3, 0].into_iter().enumerate() {
            guard.record_with(&key, &format!("tap:{index}"), Outcome::Changed, Some(frame));
        }
        let verdict = guard.check(&key, "tap:next");
        assert_eq!(verdict.code(), Some(SAME_STATE));
        assert!(!verdict.is_refusal());
    }

    #[test]
    fn a_screen_never_left_alone_is_a_repeat_not_a_return() {
        let guard = Guard::default();
        let key = key();
        guard.record_with(&key, "tap:0", Outcome::Changed, Some(0));
        for index in 0..10 {
            guard.record_with(&key, &format!("tap:{}", index + 1), Outcome::Changed, Some(0));
        }
        let verdict = guard.check(&key, "tap:next");
        assert_ne!(verdict.code(), Some(SAME_STATE));
    }

    #[test]
    fn a_long_run_without_a_new_screen_is_refused_under_a_short_budget() {
        let guard = Guard::default();
        let key = key();
        guard.record_with(&key, "tap:0", Outcome::Changed, Some(0));
        for index in 0..24 {
            guard.record_with(&key, &format!("tap:{}", index + 1), Outcome::Changed, Some(0));
        }
        // 24 actions without a new screen; budget 48 puts the stop at 24.
        let verdict = guard.check_in(&key, "tap:next", Scope { budget: Some(48) });
        assert_eq!(verdict.code(), Some(NO_NEW_STATE));
        assert!(verdict.is_refusal());
    }

    #[test]
    fn the_budget_stretches_the_late_stops() {
        assert_eq!(same_state_ladder(None)[2], 15);
        assert_eq!(same_state_ladder(Some(20))[2], 10);
        assert_eq!(same_state_ladder(Some(200))[2], 50);
        assert_eq!(cycle_ladder(None)[2], 12);
        assert_eq!(cycle_ladder(Some(20))[2], 6);
        assert_eq!(no_new_state_ladder(None)[2], 30);
        assert_eq!(no_new_state_ladder(Some(20))[2], 24);
    }

    // ── bookkeeping ────────────────────────────────────────────────────────

    #[test]
    fn forget_clears_a_bucket() {
        let guard = Guard::default();
        let key = key();
        for _ in 0..12 {
            guard.record(&key, "tap:x", Outcome::Changed);
        }
        assert!(guard.check(&key, "tap:x").is_refusal());
        guard.forget(&key);
        assert_eq!(guard.check(&key, "tap:x"), Verdict::Clear);
    }

    #[test]
    fn one_conversation_does_not_count_another_ones_taps() {
        let guard = Guard::default();
        let one = key();
        let other = key();
        for _ in 0..12 {
            guard.record(&one, "tap:x", Outcome::Changed);
        }
        assert!(guard.check(&one, "tap:x").is_refusal());
        assert_eq!(guard.check(&other, "tap:x"), Verdict::Clear);
    }
}
