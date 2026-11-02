//! The speed a person would work at, for actions that reach outward.
//!
//! A tap that sends a message or places an order is one tap; a loop that
//! repeats it is what gets an account flagged. The guard is time, not a count:
//! between two such actions in the same app, wait long enough that the pattern
//! looks like someone working through a list rather than a script hammering a
//! button.
//!
//! The shape is Mika's `OutwardPacing`, and the numbers are its numbers:
//!
//! - the first action in an app does not wait;
//! - the next one lands at least a random gap after the previous one, the
//!   window growing with how many have been done recently (8–20 s, growing
//!   0.5/1 s per action, capped at 30 s);
//! - the gap is measured from when the previous action *finished*, so reading
//!   the screen and thinking in between counts toward it — only the shortfall
//!   is waited out;
//! - each app keeps its own history; the window is 30 minutes, so a task that
//!   comes back after lunch starts fresh.
//!
//! This module is the pure part: a clock and a bucket of counts. The waiting
//! itself is the caller's, so a test can advance time instead of sleeping, and
//! so nothing here holds the process open.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Timings, in the terms a person's pace is described.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Gap range after the first action in an app.
    pub first_gap: (Duration, Duration),
    /// How much each further action in the window adds to the low and high
    /// ends of the range.
    pub per_action: (Duration, Duration),
    /// No gap is ever longer than this.
    pub max_gap: Duration,
    /// How far back "recently" reaches.
    pub window: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            first_gap: (Duration::from_secs(8), Duration::from_secs(20)),
            per_action: (Duration::from_millis(500), Duration::from_secs(1)),
            max_gap: Duration::from_secs(30),
            window: Duration::from_secs(30 * 60),
        }
    }
}

/// One completed outward action.
#[derive(Debug, Clone)]
struct Done {
    package: String,
    at: Instant,
    gap: Duration,
}

/// Per-app history of outward actions, and the gap each one bought.
#[derive(Debug)]
pub struct Pacing {
    limits: Limits,
    /// `None` means the pacing is off; every answer is zero.
    enabled: bool,
    done: Mutex<VecDeque<Done>>,
}

impl Default for Pacing {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Pacing {
    /// A pacing that waits, with the given limits.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            enabled: true,
            done: Mutex::new(VecDeque::new()),
        }
    }

    /// A pacing that never waits, for a deployment that turned it off.
    pub fn off() -> Self {
        Self {
            limits: Limits::default(),
            enabled: false,
            done: Mutex::new(VecDeque::new()),
        }
    }

    /// The process-wide history.
    ///
    /// One phone is one place: two agents working different tasks in the same
    /// app should share the pace, or two slow agents are one fast one. The
    /// lock only guards the deque; it is never held across an await.
    pub fn global() -> &'static Pacing {
        static PACING: std::sync::OnceLock<Pacing> = std::sync::OnceLock::new();
        PACING.get_or_init(Pacing::default)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// How long still owed before the next outward action in `package`.
    ///
    /// Zero means go now. Measured from the *end* of the previous action, so a
    /// turn that spent twenty seconds reading a page owes nothing.
    pub fn wait_before(&self, package: &str, now: Instant) -> Duration {
        if !self.enabled {
            return Duration::ZERO;
        }
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut done, now);
        let Some(last) = done.iter().rev().find(|d| d.package == package) else {
            return Duration::ZERO;
        };
        let owed = last
            .gap
            .saturating_sub(now.saturating_duration_since(last.at));
        owed.min(self.limits.max_gap)
    }

    /// Record that an outward action in `package` just finished.
    ///
    /// The gap it buys is fixed now, from how many were done in the window, so
    /// two callers racing cannot both read the same count and both get the
    /// first-action freedom.
    pub fn note_done(&self, package: &str, now: Instant) {
        if !self.enabled {
            return;
        }
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        self.prune(&mut done, now);
        let count = done.iter().filter(|d| d.package == package).count() + 1;
        let gap = self.gap_after(count);
        done.push_back(Done {
            package: package.to_string(),
            at: now,
            gap,
        });
        // The window bounds the deque in practice; the cap is for a task that
        // touches many apps in a short time, where the window prunes nothing.
        while done.len() > 256 {
            done.pop_front();
        }
    }

    /// The gap owed after `n` outward actions in the window.
    fn gap_after(&self, n: usize) -> Duration {
        let extra = Duration::from_secs(n.saturating_sub(1) as u64);
        let lo = (self.limits.first_gap.0 + extra.mul_f64(self.limits.per_action.0.as_secs_f64()))
            .min(self.limits.max_gap);
        let hi = (self.limits.first_gap.1 + extra.mul_f64(self.limits.per_action.1.as_secs_f64()))
            .min(self.limits.max_gap);
        if lo >= hi {
            return hi;
        }
        // A uniform draw in the window, from the same generator the rest of
        // the crate uses. Unpredictability matters only in that a fixed rhythm
        // would itself be the pattern; it is not a security boundary.
        lo + Duration::from_millis(rand::random_range(0..=(hi - lo).as_millis() as u64))
    }

    /// Wait out the shortfall, then record the action as finished.
    ///
    /// The wait happens here, at the end of the outward action, rather than at
    /// the start of the next one. This tool reply is what the model sees before
    /// it chooses its next target: a pause at the start of the *next* action
    /// would come after the model had already picked something on a screen
    /// that was still moving. The pause belongs between the decision and the
    /// following observation.
    ///
    /// The record is stamped after the sleep, so the gap owed by the action
    /// after next is measured from when this one's pause ended.
    pub async fn pace(&self, package: &str) -> Duration {
        if !self.enabled {
            return Duration::ZERO;
        }
        let owed = self.wait_before(package, Instant::now());
        if !owed.is_zero() {
            tokio::time::sleep(owed).await;
        }
        self.note_done(package, Instant::now());
        owed
    }

    fn prune(&self, done: &mut VecDeque<Done>, now: Instant) {
        while let Some(front) = done.front() {
            if now.saturating_duration_since(front.at) > self.limits.window {
                done.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> Limits {
        Limits {
            first_gap: (Duration::from_millis(100), Duration::from_millis(200)),
            per_action: (Duration::from_millis(10), Duration::from_millis(20)),
            max_gap: Duration::from_secs(1),
            window: Duration::from_secs(60),
        }
    }

    #[test]
    fn the_first_action_in_an_app_does_not_wait() {
        let pacing = Pacing::new(fast());
        let now = Instant::now();
        assert_eq!(pacing.wait_before("com.slack", now), Duration::ZERO);
    }

    #[test]
    fn a_second_action_waits_out_the_gap() {
        let pacing = Pacing::new(fast());
        let now = Instant::now();
        pacing.note_done("com.slack", now);
        let owed = pacing.wait_before("com.slack", now);
        assert!(owed >= Duration::from_millis(100), "{owed:?}");
        assert!(owed <= Duration::from_millis(200), "{owed:?}");
    }

    #[test]
    fn time_spent_reading_counts_toward_the_gap() {
        let pacing = Pacing::new(fast());
        let now = Instant::now();
        pacing.note_done("com.slack", now);
        // Twenty seconds of a turn passing is most of an 8–20 s gap.
        let later = now + Duration::from_millis(150);
        let owed = pacing.wait_before("com.slack", later);
        assert!(owed < Duration::from_millis(60), "{owed:?}");
    }

    #[test]
    fn enough_time_means_no_wait_at_all() {
        let pacing = Pacing::new(fast());
        let now = Instant::now();
        pacing.note_done("com.slack", now);
        assert_eq!(pacing.wait_before("com.slack", now + Duration::from_secs(5)), Duration::ZERO);
    }

    #[test]
    fn apps_keep_their_own_history() {
        let pacing = Pacing::new(fast());
        let now = Instant::now();
        pacing.note_done("com.slack", now);
        assert_eq!(pacing.wait_before("com.wechat", now), Duration::ZERO);
    }

    #[test]
    fn the_gap_grows_with_repetition_and_stops_at_the_cap() {
        let pacing = Pacing::new(fast());
        let limits = fast();
        for n in 1..=40_usize {
            // Each draw is random inside its own band, so the check is against
            // the band rather than against the previous draw — two adjacent
            // bands overlap, and a sequence of draws is not monotonic.
            let extra = (n - 1) as u32;
            let lo = (limits.first_gap.0 + limits.per_action.0 * extra).min(limits.max_gap);
            let hi = (limits.first_gap.1 + limits.per_action.1 * extra).min(limits.max_gap);
            let gap = pacing.gap_after(n);
            assert!(gap >= lo, "gap {gap:?} below {lo:?} at {n}");
            assert!(gap <= hi, "gap {gap:?} above {hi:?} at {n}");
        }
        assert_eq!(pacing.gap_after(10_000), limits.max_gap);
    }

    #[test]
    fn an_action_after_the_window_starts_fresh() {
        let pacing = Pacing::new(fast());
        let now = Instant::now();
        pacing.note_done("com.slack", now);
        assert_eq!(
            pacing.wait_before("com.slack", now + Duration::from_secs(120)),
            Duration::ZERO,
            "the window is 60 s in these limits"
        );
    }

    #[test]
    fn being_off_never_waits() {
        let pacing = Pacing::off();
        let now = Instant::now();
        pacing.note_done("com.slack", now);
        assert_eq!(pacing.wait_before("com.slack", now), Duration::ZERO);
    }

    #[test]
    fn the_defaults_are_the_ones_on_the_phone() {
        let limits = Limits::default();
        assert_eq!(limits.first_gap, (Duration::from_secs(8), Duration::from_secs(20)));
        assert_eq!(limits.max_gap, Duration::from_secs(30));
        assert_eq!(limits.window, Duration::from_secs(30 * 60));
    }
}
