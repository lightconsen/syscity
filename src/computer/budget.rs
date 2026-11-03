//! One time budget for one step, shared by every call the step makes.
//!
//! A computer-use step is a ladder of rungs, and each rung is itself several
//! calls: a baseline screenshot, a tree read, the dispatch, a settle loop that
//! sleeps and captures until the screen stops changing, a verification read.
//! Each of those used to carry its own magic number — `unwrap_or(15)` here,
//! `Duration::from_secs(10)` there — so the only real ceiling was the outer
//! tool timeout, which fires *during* a settle or a read and turns into an
//! opaque "tool timed out".
//!
//! A [`StepBudget`] is the opposite: the caller says how long the whole step
//! may take, and every call asks it how much is left. A settle loop stops
//! cleanly at the deadline and reports what it has; a wait is capped rather
//! than started and then killed; the ladder does not start another rung with
//! no time to finish it. What the step then reports is a *refusal with a
//! budget*, not a panic-shaped timeout from somewhere above.

use std::time::Duration;

use tokio::time::Instant;

use crate::computer::ComputerError;

/// How long one step may take, from its first call to its last.
///
/// `None` means unlimited — the historical behaviour, kept for callers (and
/// tests) that do not pass a budget. Every method is a no-op in that case.
#[derive(Debug, Clone, Copy)]
pub struct StepBudget {
    started: Instant,
    total: Option<Duration>,
}

impl StepBudget {
    /// A budget of `total`.
    pub fn new(total: Duration) -> Self {
        Self {
            started: Instant::now(),
            total: Some(total),
        }
    }

    /// No ceiling. The caller keeps whatever timeout it had before.
    pub fn unlimited() -> Self {
        Self {
            started: Instant::now(),
            total: None,
        }
    }

    /// The budget the step started with, when it has one.
    pub fn total(&self) -> Option<Duration> {
        self.total
    }

    /// What is left. `None` when unlimited; `Some(0)` when spent.
    pub fn remaining(&self) -> Option<Duration> {
        self.total
            .map(|total| total.saturating_sub(self.started.elapsed()))
    }

    /// Whether the budget is spent.
    ///
    /// An unlimited budget is never exhausted: "no ceiling" must not be
    /// confused with "no time left".
    pub fn is_exhausted(&self) -> bool {
        self.remaining().is_some_and(|left| left.is_zero())
    }

    /// The shorter of `requested` and what is left.
    ///
    /// This is what makes a per-call timeout stop being a magic number: the
    /// call asks for what it needs, and gets no more than the step can pay.
    pub fn cap(&self, requested: Duration) -> Duration {
        match self.remaining() {
            Some(left) => requested.min(left),
            None => requested,
        }
    }

    /// Elapsed time since the step started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// A description of the budget for evidence and result data.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "total_ms": self.total.map(|t| t.as_millis() as u64),
            "elapsed_ms": self.elapsed().as_millis() as u64,
            "remaining_ms": self.remaining().map(|t| t.as_millis() as u64),
        })
    }
}

/// Run `future`, giving up when the budget runs out.
///
/// The error names the budget rather than a bare "timed out": the caller that
/// set the ceiling is the one who can raise it.
pub async fn within<F: std::future::Future>(
    budget: &StepBudget,
    what: &str,
    future: F,
) -> Result<F::Output, ComputerError> {
    let Some(left) = budget.remaining() else {
        return Ok(future.await);
    };
    match tokio::time::timeout(left, future).await {
        Ok(output) => Ok(output),
        Err(_) => Err(ComputerError::BudgetExhausted(format!(
            "{what} needed more than the {} ms the step had left",
            budget
                .total()
                .map(|t| t.as_millis().to_string())
                .unwrap_or_else(|| "unlimited".to_string()),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlimited_budget_never_runs_out() {
        let budget = StepBudget::unlimited();
        assert_eq!(budget.total(), None);
        assert_eq!(budget.remaining(), None);
        assert!(!budget.is_exhausted());
        assert_eq!(budget.cap(Duration::from_secs(30)), Duration::from_secs(30));
        assert_eq!(budget.summary()["total_ms"], serde_json::Value::Null);
    }

    #[test]
    fn a_cap_never_exceeds_what_is_left() {
        let budget = StepBudget::new(Duration::from_secs(10));
        // Strictly less than the total: some of it has already been spent by
        // the time the first call asks, so the cap is what is left now.
        let capped = budget.cap(Duration::from_secs(30));
        assert!(capped <= Duration::from_secs(10), "{capped:?}");
        assert!(capped > Duration::from_secs(9), "{capped:?}");
        assert!(!budget.is_exhausted());
        assert!(budget.remaining().expect("budget") <= Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_spent_budget_is_exhausted_and_caps_to_zero() {
        let budget = StepBudget::new(Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(budget.is_exhausted());
        assert_eq!(budget.cap(Duration::from_secs(30)), Duration::ZERO);
        assert_eq!(budget.summary()["remaining_ms"], 0);
    }

    /// The ceiling applies to the work, not to the reporting: an operation
    /// that finishes inside the budget returns its value untouched.
    #[tokio::test]
    async fn work_inside_the_budget_returns_its_value() {
        let budget = StepBudget::new(Duration::from_secs(5));
        let value = within(&budget, "a read", async { 41 + 1 })
            .await
            .expect("in time");
        assert_eq!(value, 42);
    }

    /// An operation that outlasts the budget fails with a budget error, not
    /// a generic timeout — the message names the step's ceiling.
    #[tokio::test]
    async fn work_past_the_budget_is_a_budget_error() {
        let budget = StepBudget::new(Duration::from_millis(1));
        let err = within(&budget, "a read", async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            1
        })
        .await
        .expect_err("past the budget");
        assert!(matches!(err, ComputerError::BudgetExhausted(_)), "{err:?}");
        assert!(err.to_string().contains("a read"), "{err}");
    }

    /// An unlimited budget must not impose a timeout at all: an operation
    /// longer than any magic number still completes.
    #[tokio::test]
    async fn an_unlimited_budget_does_not_time_out() {
        let budget = StepBudget::unlimited();
        let value = within(&budget, "a read", async { 7 })
            .await
            .expect("no ceiling");
        assert_eq!(value, 7);
    }
}
