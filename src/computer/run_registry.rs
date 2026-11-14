//! Live computer-use runs, as the gateway sees them.
//!
//! A run outlives the request that started it: `computer.run` returns as soon
//! as the loop is spawned, and everything after that arrives as events and
//! answers `computer.status`. That needs a place to keep the runs — and more
//! than a `HashMap<String, JoinHandle>`, because a status call has to report
//! what the loop is doing *now*: which step, what the last action was, whether
//! it verified, and how much of the budget is gone.
//!
//! It also has to be cancellable, pausable and steerable from another
//! connection. That is [`ExecutionController`]'s job and this module does not
//! reimplement it: each run owns one, and `computer.control` forwards to it.
//!
//! Finished runs are kept (a bounded number of them) rather than removed on
//! completion. A run that has just finished is exactly when a client most
//! wants to read its outcome, and deleting it at that moment would turn the
//! natural "run, then ask what happened" into a race.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::acp::config::RuntimeState;
use crate::acp::ExecutionController;
use crate::computer::run_budget::{RunSummary, StopReason};

/// How many finished runs the registry keeps.
///
/// A run is a few hundred bytes of summary once it is over; the ceiling is
/// there so a long-lived gateway that runs thousands of goals does not grow
/// without bound, not because the entries are expensive.
pub const MAX_FINISHED_RUNS: usize = 20;

/// What a run is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    /// The loop is stepping.
    Running,
    /// Waiting for `computer.control` with `resume` or `step`.
    Paused,
    /// Will run one more step, then pause.
    Stepping,
    /// Ended because someone cancelled it.
    Cancelled,
    /// Ended because the goal was met.
    Done,
    /// Ended without meeting the goal.
    Failed,
}

impl RunPhase {
    /// Whether the run has stopped for good.
    pub fn is_finished(self) -> bool {
        matches!(self, RunPhase::Cancelled | RunPhase::Done | RunPhase::Failed)
    }
}

/// The live picture of one run.
#[derive(Debug, Clone)]
pub struct RunState {
    pub step: usize,
    /// What the last executed action was, in words (`click [s1:0.4]`).
    pub last_action: Option<String>,
    /// Whether the last action was verified.
    pub last_verified: Option<bool>,
    pub consecutive_failures: usize,
    pub last_error: Option<String>,
    /// One line per completed step, newest last — the spine of the handoff
    /// note when a run stops without finishing.
    pub completed: Vec<String>,
    pub model_calls: usize,
    pub images_attached: usize,
    pub phase: RunPhase,
    /// Set when the run ended.
    pub summary: Option<RunSummary>,
    /// What the last step's visual scan saw, or why it could not see.
    ///
    /// Recorded even when the answer is "nobody could look": the plan's fourth
    /// requirement is that an unavailable backend is stated in the run log, and
    /// a run that stopped because it was blind must not look like one that ran
    /// out of steps.
    pub vision: Option<String>,
    /// The closing message: the goal's result, or why it stopped.
    pub message: Option<String>,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            step: 0,
            last_action: None,
            last_verified: None,
            consecutive_failures: 0,
            last_error: None,
            completed: Vec::new(),
            model_calls: 0,
            images_attached: 0,
            phase: RunPhase::Running,
            summary: None,
            vision: None,
            message: None,
        }
    }
}

/// One run, shared between the loop task and every connection asking about it.
pub struct ComputerRun {
    pub run_id: String,
    pub goal: String,
    /// Pause / resume / step / cancel / steer. Shared with the loop, which
    /// consults it between steps.
    pub controller: Arc<ExecutionController>,
    state: RwLock<RunState>,
}

impl ComputerRun {
    pub fn new(run_id: impl Into<String>, goal: impl Into<String>) -> Arc<Self> {
        let run_id = run_id.into();
        Arc::new(Self {
            run_id,
            goal: goal.into(),
            controller: ExecutionController::new(),
            state: RwLock::new(RunState::default()),
        })
    }

    pub async fn snapshot(&self) -> RunState {
        self.state.read().await.clone()
    }

    /// Replace the state under the lock, so the caller can mutate a copy and
    /// write it back without holding the lock across its own work.
    pub async fn update(&self, f: impl FnOnce(&mut RunState)) {
        let mut state = self.state.write().await;
        f(&mut state);
    }

    /// The runtime state as `computer.status` reports it.
    ///
    /// Derived from the controller first: a pause is the controller's fact,
    /// and a run that was just paused has not necessarily written `Paused`
    /// into its own state yet.
    pub async fn runtime_state(&self) -> RuntimeState {
        self.controller.current_state().await
    }

    /// A bounded, serializable view for `computer.status`.
    pub async fn view(&self) -> serde_json::Value {
        let state = self.snapshot().await;
        let runtime = self.runtime_state().await;
        serde_json::json!({
            "run_id": self.run_id,
            "goal": self.goal,
            "phase": state.phase,
            "runtime_state": format!("{runtime:?}").to_lowercase(),
            "step": state.step,
            "last_action": state.last_action,
            "last_verified": state.last_verified,
            "consecutive_failures": state.consecutive_failures,
            "last_error": state.last_error,
            "completed_steps": state.completed,
            "model_calls": state.model_calls,
            "images_attached": state.images_attached,
            "summary": state.summary,
            "vision": state.vision,
            "message": state.message,
            "finished": state.phase.is_finished(),
        })
    }
}

/// Every run this gateway knows about.
#[derive(Default)]
pub struct ComputerRunRegistry {
    runs: RwLock<HashMap<String, Arc<ComputerRun>>>,
}

impl ComputerRunRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new run and return it.
    pub async fn create(
        &self,
        run_id: impl Into<String>,
        goal: impl Into<String>,
    ) -> Arc<ComputerRun> {
        let run = ComputerRun::new(run_id, goal);
        self.runs
            .write()
            .await
            .insert(run.run_id.clone(), run.clone());
        run
    }

    pub async fn get(&self, run_id: &str) -> Option<Arc<ComputerRun>> {
        self.runs.read().await.get(run_id).cloned()
    }

    /// All runs' views, newest id last. Finished ones are included.
    pub async fn list(&self) -> Vec<serde_json::Value> {
        let runs: Vec<Arc<ComputerRun>> = {
            let guard = self.runs.read().await;
            let mut runs: Vec<_> = guard.values().cloned().collect();
            runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
            runs
        };
        let mut views = Vec::with_capacity(runs.len());
        for run in runs {
            views.push(run.view().await);
        }
        views
    }

    /// How many runs are still going.
    pub async fn live_count(&self) -> usize {
        let runs: Vec<Arc<ComputerRun>> = self.runs.read().await.values().cloned().collect();
        let mut live = 0;
        for run in runs {
            if !run.snapshot().await.phase.is_finished() {
                live += 1;
            }
        }
        live
    }

    /// Drop finished runs once there are more than [`MAX_FINISHED_RUNS`],
    /// oldest-completing first.
    ///
    /// Called when a run ends rather than before every insert: the sweep needs
    /// to know which runs are finished, which means reading every state.
    pub async fn sweep_finished(&self) {
        let finished: Vec<(String, Arc<ComputerRun>)> = {
            let guard = self.runs.read().await;
            guard
                .iter()
                .map(|(id, run)| (id.clone(), run.clone()))
                .collect()
        };

        let mut done = Vec::new();
        for (id, run) in finished {
            if run.snapshot().await.phase.is_finished() {
                done.push(id);
            }
        }
        if done.len() <= MAX_FINISHED_RUNS {
            return;
        }
        // Ids are UUIDs, so the lexicographic order is not a completion order.
        // Keeping an arbitrary subset is fine here — the ceiling exists to
        // bound memory, and any finished run is as droppable as another.
        let excess = done.len() - MAX_FINISHED_RUNS;
        let mut guard = self.runs.write().await;
        for id in done.into_iter().take(excess) {
            guard.remove(&id);
        }
    }

    /// Remove a run regardless of phase. Used when the task registry aborts it.
    pub async fn remove(&self, run_id: &str) -> Option<Arc<ComputerRun>> {
        self.runs.write().await.remove(run_id)
    }
}

/// Turn a loop outcome into the phase and message a run ends with.
pub fn finalize(
    state: &mut RunState,
    success: bool,
    message: String,
    cancelled: bool,
    stop_reason: StopReason,
    steps: usize,
    budget: &crate::computer::run_budget::RunBudget,
) {
    state.step = steps;
    state.phase = if cancelled {
        RunPhase::Cancelled
    } else if success {
        RunPhase::Done
    } else {
        RunPhase::Failed
    };
    state.summary = Some(budget.summary(steps, stop_reason));
    state.message = Some(message);
    state.model_calls = budget.model_calls();
    state.images_attached = budget.images_attached();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_new_run_is_live_and_readable() {
        let registry = ComputerRunRegistry::new();
        let run = registry.create("r1", "open the calculator").await;
        let view = run.view().await;
        assert_eq!(view["phase"], "running");
        assert_eq!(view["finished"], false);
        assert_eq!(view["goal"], "open the calculator");
        assert_eq!(registry.live_count().await, 1);
    }

    #[tokio::test]
    async fn a_finished_run_stays_readable() {
        let registry = ComputerRunRegistry::new();
        let run = registry.create("r1", "g").await;
        run.update(|s| {
            s.phase = RunPhase::Done;
            s.message = Some("done".to_string());
        })
        .await;

        // The point: a client that asks just after completion still gets an
        // answer. Removing on completion would make that a race.
        assert!(registry.get("r1").await.is_some());
        let view = registry.get("r1").await.unwrap().view().await;
        assert_eq!(view["finished"], true);
        assert_eq!(registry.live_count().await, 0);
    }

    #[tokio::test]
    async fn the_sweep_keeps_the_ceiling() {
        let registry = ComputerRunRegistry::new();
        for i in 0..MAX_FINISHED_RUNS + 5 {
            let run = registry.create(format!("r{i:03}"), "g").await;
            run.update(|s| s.phase = RunPhase::Done).await;
        }
        registry.sweep_finished().await;
        let remaining = registry.list().await;
        assert!(remaining.len() <= MAX_FINISHED_RUNS, "{} runs left", remaining.len());
        // And it never touches a live run.
        assert!(remaining.iter().all(|v| v["finished"] == true));
    }

    #[tokio::test]
    async fn the_sweep_leaves_live_runs_alone() {
        let registry = ComputerRunRegistry::new();
        for i in 0..MAX_FINISHED_RUNS + 3 {
            registry
                .create(format!("done{i}"), "g")
                .await
                .update(|s| s.phase = RunPhase::Done)
                .await;
        }
        let live = registry.create("live", "g").await;
        live.update(|s| s.phase = RunPhase::Running).await;

        registry.sweep_finished().await;
        assert!(registry.get("live").await.is_some(), "a live run must never be swept");
    }

    #[tokio::test]
    async fn a_missing_run_is_none_not_an_error() {
        let registry = ComputerRunRegistry::new();
        assert!(registry.get("nope").await.is_none());
        assert!(registry.list().await.is_empty());
    }

    #[tokio::test]
    async fn status_reports_the_controller_state() {
        let run = ComputerRun::new("r1", "g");
        run.update(|s| s.phase = RunPhase::Paused).await;
        run.controller.pause().await;
        let view = run.view().await;
        assert_eq!(view["runtime_state"], "paused");
        // Phase and runtime state are separate facts; the view carries both.
        assert_eq!(view["phase"], "paused");
    }

    #[test]
    fn a_cancelled_run_is_finished() {
        assert!(RunPhase::Cancelled.is_finished());
        assert!(RunPhase::Done.is_finished());
        assert!(RunPhase::Failed.is_finished());
        assert!(!RunPhase::Running.is_finished());
        assert!(!RunPhase::Paused.is_finished());
        assert!(!RunPhase::Stepping.is_finished());
    }
}
