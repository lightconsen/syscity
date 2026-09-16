//! Live cost guard for the agent loop.
//!
//! `CostGuard` tracks daily dollar spend (in cents) and hourly action rate,
//! exposing an atomic `budget_exceeded` flag that is checked before every
//! provider call in `Agent::get_completion()`.
//!
//! Counters auto-reset every 24 h (daily) and 1 h (hourly) without requiring
//! a cron job — including *after* a limit has tripped: the reset is evaluated
//! on the read path (`is_exceeded`), because a tripped guard refuses every
//! provider call and so would otherwise never reach the write path that used
//! to be the only place the windows were checked. `reset_exceeded` remains for
//! an operator who wants to clear it immediately (`cost.reset` over WS).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Live spending and action-rate tracker.
///
/// All methods are cheaply callable from hot async paths — they use
/// atomics and only lock a `Mutex` for the rare reset check.
pub struct CostGuard {
    /// Accumulated spend today, in cents.
    daily_cents: AtomicU64,
    /// Maximum daily spend in cents (0 = unlimited).
    pub daily_limit_cents: u64,
    /// Actions taken in the current hour.
    hourly_actions: AtomicU64,
    /// Maximum hourly actions (0 = unlimited).
    pub hourly_action_limit: u64,
    /// Set to `true` when any limit is crossed.
    pub budget_exceeded: AtomicBool,
    /// Timestamp of the last daily reset.
    last_daily_reset: Mutex<SystemTime>,
    /// Instant of the last hourly reset.
    last_hourly_reset: Mutex<Instant>,
}

impl std::fmt::Debug for CostGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CostGuard")
            .field("daily_cents", &self.daily_cents.load(Ordering::Relaxed))
            .field("daily_limit_cents", &self.daily_limit_cents)
            .field("hourly_actions", &self.hourly_actions.load(Ordering::Relaxed))
            .field("hourly_action_limit", &self.hourly_action_limit)
            .field("budget_exceeded", &self.budget_exceeded.load(Ordering::Relaxed))
            .finish()
    }
}

impl CostGuard {
    /// Create a new `CostGuard` wrapped in an `Arc`.
    ///
    /// Pass `0` to either limit to disable that guard.
    pub fn new(daily_limit_cents: u64, hourly_action_limit: u64) -> Arc<Self> {
        Arc::new(Self {
            daily_cents: AtomicU64::new(0),
            daily_limit_cents,
            hourly_actions: AtomicU64::new(0),
            hourly_action_limit,
            budget_exceeded: AtomicBool::new(false),
            last_daily_reset: Mutex::new(SystemTime::now()),
            last_hourly_reset: Mutex::new(Instant::now()),
        })
    }

    /// Returns `true` if any spending or rate limit has been exceeded.
    ///
    /// Evaluates the windows first. This is load-bearing: once the guard trips,
    /// every provider call is refused, so `record_usage` — and with it the only
    /// other call to [`Self::maybe_reset`] — never runs again. Without the
    /// check here the hourly and daily rollovers could never clear the flag,
    /// and a tripped guard stayed tripped until the process restarted.
    pub fn is_exceeded(&self) -> bool {
        self.maybe_reset();
        self.budget_exceeded.load(Ordering::Acquire)
    }

    /// Record the token usage for one provider call.
    ///
    /// Increments the hourly action counter and accumulates estimated cost.
    /// Trips `budget_exceeded` if a limit is crossed.
    pub fn record_usage(&self, input_tokens: u64, output_tokens: u64, model: &str) {
        self.maybe_reset();

        let (input_cpm, output_cpm) = Self::pricing_for_model(model);
        // cpm = cents per million tokens
        let cost_cents = (input_tokens * input_cpm + output_tokens * output_cpm) / 1_000_000;

        let new_daily = self.daily_cents.fetch_add(cost_cents, Ordering::AcqRel) + cost_cents;
        let new_hourly = self.hourly_actions.fetch_add(1, Ordering::AcqRel) + 1;

        let daily_exceeded = self.daily_limit_cents > 0 && new_daily >= self.daily_limit_cents;
        let hourly_exceeded =
            self.hourly_action_limit > 0 && new_hourly >= self.hourly_action_limit;

        if daily_exceeded || hourly_exceeded {
            self.budget_exceeded.store(true, Ordering::Release);
            tracing::warn!(
                daily_cents = new_daily,
                daily_limit = self.daily_limit_cents,
                hourly_actions = new_hourly,
                hourly_limit = self.hourly_action_limit,
                "CostGuard: budget limit reached"
            );
        }
    }

    /// Current daily spend in cents.
    pub fn daily_spend_cents(&self) -> u64 {
        self.daily_cents.load(Ordering::Acquire)
    }

    /// Number of actions taken in the current hour.
    pub fn hourly_action_count(&self) -> u64 {
        self.hourly_actions.load(Ordering::Acquire)
    }

    /// Manually reset the exceeded flag (e.g. after a config change).
    ///
    /// Clears only the latch: the window's counters are untouched, so if the
    /// limit was not also raised, the next successful call trips it again. Use
    /// [`Self::clear_and_rebase`] to start a fresh window.
    pub fn reset_exceeded(&self) {
        self.budget_exceeded.store(false, Ordering::Release);
    }

    /// Clear the latch *and* start a fresh window.
    ///
    /// This is the operator's "let me work again" path (`cost.reset` over WS):
    /// zeroing the counters is what makes it more than a one-call reprieve. It
    /// overrides a guardrail deliberately, which is why it is reachable only by
    /// an explicit `write`-scoped request — the agent itself never calls it.
    pub fn clear_and_rebase(&self) {
        self.daily_cents.store(0, Ordering::Release);
        self.hourly_actions.store(0, Ordering::Release);
        *self
            .last_daily_reset
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = SystemTime::now();
        *self
            .last_hourly_reset
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.budget_exceeded.store(false, Ordering::Release);
    }

    // ── Private ───────────────────────────────────────────────────────────────

    /// Reset daily / hourly counters if the window has elapsed.
    fn maybe_reset(&self) {
        // Daily reset
        let mut last_daily = self
            .last_daily_reset
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Ok(elapsed) = last_daily.elapsed() {
            if elapsed >= Duration::from_secs(86_400) {
                self.daily_cents.store(0, Ordering::Release);
                // Only clear the flag if no hourly limit is also tripped.
                if self.hourly_action_limit == 0 {
                    self.budget_exceeded.store(false, Ordering::Release);
                }
                *last_daily = SystemTime::now();
            }
        }
        drop(last_daily);

        // Hourly reset
        let mut last_hourly = self
            .last_hourly_reset
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last_hourly.elapsed() >= Duration::from_secs(3_600) {
            self.hourly_actions.store(0, Ordering::Release);
            // Re-check whether the daily limit is still exceeded.
            let daily_ok = self.daily_limit_cents == 0
                || self.daily_cents.load(Ordering::Acquire) < self.daily_limit_cents;
            if daily_ok {
                self.budget_exceeded.store(false, Ordering::Release);
            }
            *last_hourly = Instant::now();
        }
    }

    /// Returns (input_cents_per_million, output_cents_per_million) for the
    /// named model.  Values are approximate and intended for budget guardrails,
    /// not billing accuracy.
    fn pricing_for_model(model: &str) -> (u64, u64) {
        let m = model.to_lowercase();
        if m.contains("opus") {
            (1_500, 7_500)
        } else if m.contains("sonnet") {
            (300, 1_500)
        } else if m.contains("haiku") {
            (25, 125)
        } else if m.contains("gpt-4o") {
            (250, 1_000)
        } else if m.contains("gpt-4") {
            (1_000, 3_000)
        } else if m.contains("gpt-3.5") {
            (50, 150)
        } else {
            // Conservative default
            (300, 1_500)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cost_guard_no_limits() {
        let guard = CostGuard::new(0, 0);
        guard.record_usage(1_000_000, 500_000, "claude-3-sonnet");
        assert!(!guard.is_exceeded());
    }

    #[test]
    fn test_cost_guard_daily_limit() {
        // Limit of 1 cent
        let guard = CostGuard::new(1, 0);
        // claude-sonnet: 300 cpm input → 1M tokens = 300 cents > 1 cent limit
        guard.record_usage(1_000_000, 0, "claude-3-sonnet");
        assert!(guard.is_exceeded());
    }

    #[test]
    fn test_cost_guard_hourly_limit() {
        let guard = CostGuard::new(0, 2);
        guard.record_usage(100, 100, "claude-3-haiku");
        assert!(!guard.is_exceeded());
        guard.record_usage(100, 100, "claude-3-haiku");
        assert!(guard.is_exceeded());
    }

    #[test]
    fn test_cost_guard_reset_exceeded() {
        let guard = CostGuard::new(0, 1);
        guard.record_usage(100, 100, "claude-3-haiku");
        assert!(guard.is_exceeded());
        guard.reset_exceeded();
        assert!(!guard.is_exceeded());
    }

    /// The operator path must leave the agent *working*, not grant a single
    /// call: with the counters left over the limit, the next call would
    /// re-trip immediately.
    #[test]
    fn test_clear_and_rebase_starts_a_fresh_window() {
        let guard = CostGuard::new(0, 1);
        guard.record_usage(0, 0, "claude-3-haiku");
        assert!(guard.is_exceeded(), "one call hits a limit of one");

        guard.clear_and_rebase();

        assert!(!guard.is_exceeded());
        assert_eq!(guard.hourly_action_count(), 0, "the window's count is reset");
        assert_eq!(guard.daily_spend_cents(), 0);
        // And the window is usable again: one more call fits.
        guard.record_usage(0, 0, "claude-3-haiku");
        assert!(guard.is_exceeded(), "the limit still applies to the fresh window");
    }

    /// A tripped guard must clear itself when the window rolls over.
    ///
    /// The flag is read before every provider call, and a tripped guard makes
    /// no further calls — so if the rollover were only evaluated on the *write*
    /// path, a tripped guard could never clear itself and the agent stayed
    /// wedged until the process restarted.
    #[test]
    fn test_cost_guard_clears_when_the_hour_rolls_over() {
        let guard = CostGuard {
            daily_cents: AtomicU64::new(0),
            daily_limit_cents: 0,
            hourly_actions: AtomicU64::new(2),
            hourly_action_limit: 2,
            budget_exceeded: AtomicBool::new(true),
            last_daily_reset: Mutex::new(SystemTime::now()),
            last_hourly_reset: Mutex::new(Instant::now()),
        };
        assert!(guard.is_exceeded(), "a tripped guard reports exceeded");

        // An hour passes.
        *guard.last_hourly_reset.lock().unwrap() = Instant::now() - Duration::from_secs(3_601);

        assert!(!guard.is_exceeded(), "the hour rolling over must clear the flag");
        assert_eq!(guard.hourly_action_count(), 0, "and reset the counter");
    }

    /// The same for the daily window, which only clears when no hourly limit
    /// is also tripped.
    #[test]
    fn test_cost_guard_clears_when_the_day_rolls_over() {
        let guard = CostGuard {
            daily_cents: AtomicU64::new(500),
            daily_limit_cents: 500,
            hourly_actions: AtomicU64::new(0),
            hourly_action_limit: 0,
            budget_exceeded: AtomicBool::new(true),
            last_daily_reset: Mutex::new(SystemTime::now()),
            last_hourly_reset: Mutex::new(Instant::now()),
        };
        assert!(guard.is_exceeded());

        *guard.last_daily_reset.lock().unwrap() = SystemTime::now() - Duration::from_secs(86_401);

        assert!(!guard.is_exceeded(), "the day rolling over must clear the flag");
        assert_eq!(guard.daily_spend_cents(), 0);
    }

    #[test]
    fn test_cost_guard_initial_state() {
        let guard = CostGuard::new(100, 10);
        assert!(!guard.is_exceeded());
        assert_eq!(guard.daily_spend_cents(), 0);
        assert_eq!(guard.hourly_action_count(), 0);
        assert_eq!(guard.daily_limit_cents, 100);
        assert_eq!(guard.hourly_action_limit, 10);
        assert!(!guard.budget_exceeded.load(Ordering::Relaxed));
    }

    #[test]
    fn test_cost_guard_daily_spend_cents() {
        let guard = CostGuard::new(0, 0);
        // sonnet: 300 cpm input, 1500 cpm output
        // 1M input tokens = 300 cents
        guard.record_usage(1_000_000, 0, "claude-3-sonnet");
        assert_eq!(guard.daily_spend_cents(), 300);
    }

    #[test]
    fn test_cost_guard_hourly_action_count() {
        let guard = CostGuard::new(0, 0);
        guard.record_usage(100, 100, "claude-3-haiku");
        assert_eq!(guard.hourly_action_count(), 1);
        guard.record_usage(100, 100, "claude-3-haiku");
        assert_eq!(guard.hourly_action_count(), 2);
    }

    #[test]
    fn test_cost_guard_zero_tokens() {
        let guard = CostGuard::new(0, 0);
        guard.record_usage(0, 0, "claude-3-sonnet");
        // Zero tokens = zero cost, but action is still counted
        assert_eq!(guard.daily_spend_cents(), 0);
        assert_eq!(guard.hourly_action_count(), 1);
    }

    #[test]
    fn test_pricing_for_model_opus() {
        assert_eq!(CostGuard::pricing_for_model("claude-3-opus"), (1_500, 7_500));
        assert_eq!(CostGuard::pricing_for_model("Claude-3-Opus"), (1_500, 7_500));
    }

    #[test]
    fn test_pricing_for_model_sonnet() {
        assert_eq!(CostGuard::pricing_for_model("claude-3-sonnet"), (300, 1_500));
        assert_eq!(CostGuard::pricing_for_model("claude-sonnet-4-6"), (300, 1_500));
    }

    #[test]
    fn test_pricing_for_model_haiku() {
        assert_eq!(CostGuard::pricing_for_model("claude-3-haiku"), (25, 125));
    }

    #[test]
    fn test_pricing_for_model_gpt4o() {
        assert_eq!(CostGuard::pricing_for_model("gpt-4o"), (250, 1_000));
        assert_eq!(CostGuard::pricing_for_model("GPT-4O"), (250, 1_000));
    }

    #[test]
    fn test_pricing_for_model_gpt4() {
        assert_eq!(CostGuard::pricing_for_model("gpt-4"), (1_000, 3_000));
        assert_eq!(CostGuard::pricing_for_model("gpt-4-turbo"), (1_000, 3_000));
    }

    #[test]
    fn test_pricing_for_model_gpt35() {
        assert_eq!(CostGuard::pricing_for_model("gpt-3.5-turbo"), (50, 150));
    }

    #[test]
    fn test_pricing_for_model_unknown() {
        assert_eq!(CostGuard::pricing_for_model("unknown-model"), (300, 1_500));
        assert_eq!(CostGuard::pricing_for_model(""), (300, 1_500));
    }

    #[test]
    fn test_cost_guard_exact_daily_limit() {
        // sonnet: 300 cpm input. 1M tokens costs exactly 300 cents.
        let guard = CostGuard::new(300, 0);
        guard.record_usage(1_000_000, 0, "claude-3-sonnet");
        assert!(guard.is_exceeded());
        assert_eq!(guard.daily_spend_cents(), 300);
    }

    #[test]
    fn test_cost_guard_both_limits_daily_triggers() {
        let guard = CostGuard::new(1, 100);
        // sonnet: 300 cpm → 1M tokens = 300 cents > 1 cent limit
        guard.record_usage(1_000_000, 0, "claude-3-sonnet");
        assert!(guard.is_exceeded());
        assert_eq!(guard.hourly_action_count(), 1);
    }

    #[test]
    fn test_cost_guard_both_limits_hourly_triggers() {
        let guard = CostGuard::new(10_000, 2);
        guard.record_usage(100, 100, "claude-3-haiku");
        assert!(!guard.is_exceeded());
        guard.record_usage(100, 100, "claude-3-haiku");
        assert!(guard.is_exceeded());
        // Daily spend should still be tiny (haiku is 25 cpm → 100 tokens = 0 cents due
        // to integer division)
        assert_eq!(guard.daily_spend_cents(), 0);
    }

    #[test]
    fn test_cost_guard_debug() {
        let guard = CostGuard::new(100, 10);
        guard.record_usage(1_000_000, 0, "claude-3-sonnet");
        let debug = format!("{:?}", guard);
        assert!(debug.contains("CostGuard"));
        assert!(debug.contains("daily_cents"));
        assert!(debug.contains("300"));
    }

    #[test]
    fn test_cost_guard_arc_clone() {
        let guard = CostGuard::new(100, 10);
        let guard2 = Arc::clone(&guard);
        guard.record_usage(1_000_000, 0, "claude-3-sonnet");
        assert_eq!(guard2.daily_spend_cents(), 300);
    }

    #[test]
    fn test_cost_guard_multiple_models() {
        let guard = CostGuard::new(0, 0);
        guard.record_usage(1_000_000, 0, "claude-3-opus"); // 1500 cents
        guard.record_usage(1_000_000, 0, "claude-3-sonnet"); // +300 cents
        guard.record_usage(1_000_000, 0, "claude-3-haiku"); // +25 cents
        assert_eq!(guard.daily_spend_cents(), 1_825);
        assert_eq!(guard.hourly_action_count(), 3);
    }
}
