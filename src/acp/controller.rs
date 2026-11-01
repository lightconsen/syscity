//! Execution controller for pausing, resuming, stepping, and cancelling
//! iterative agent execution.
//!
//! The controller is shared between the ACP actor loop and the agent's
//! tool-call loop via an [`Arc`]. Operators can pause execution between LLM
//! iterations, single-step through iterations, or cancel the run entirely.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;

use super::config::RuntimeState;

/// Controller for pausing / resuming / stepping execution.
///
/// Inserted into the Agent's tool-call loop so operators can pause
/// between LLM iterations.
#[derive(Debug)]
pub struct ExecutionController {
    state: RwLock<RuntimeState>,
    notify: tokio::sync::Notify,
    iteration: std::sync::atomic::AtomicUsize,
    /// Instructions that arrived mid-turn, for the next iteration to pick up.
    ///
    /// A plain `std::sync::Mutex`: every use is a push or a take, nothing is
    /// awaited while it is held, so it cannot block a runtime thread across a
    /// yield point.
    steers: std::sync::Mutex<Vec<String>>,
}

impl ExecutionController {
    /// Create a new controller in the `Idle` state.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(RuntimeState::Idle),
            notify: tokio::sync::Notify::new(),
            iteration: std::sync::atomic::AtomicUsize::new(0),
            steers: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Check if execution should proceed.
    pub async fn check_and_wait(&self) -> Result<(), &'static str> {
        loop {
            let state = *self.state.read().await;
            match state {
                RuntimeState::Idle | RuntimeState::Running => {
                    self.iteration
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
                RuntimeState::Stepping => {
                    self.iteration
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    *self.state.write().await = RuntimeState::Paused;
                    return Ok(());
                }
                RuntimeState::Paused => {
                    self.notify.notified().await;
                    continue;
                }
                RuntimeState::Cancelled => return Err("Execution cancelled by user"),
            }
        }
    }

    /// Transition to `Paused`.
    /// Wait until this execution may continue, without counting an iteration.
    ///
    /// [`check_and_wait`](Self::check_and_wait) is the *iteration* gate — it
    /// bumps the step counter, which is what the ACP view reports. A caller
    /// inside one iteration (a streaming reply, which has many chunks and no
    /// steps) needs the waiting half without the counting half.
    pub async fn wait_until_resumed(&self) -> Result<(), &'static str> {
        loop {
            let state = *self.state.read().await;
            match state {
                RuntimeState::Idle | RuntimeState::Running | RuntimeState::Stepping => {
                    return Ok(());
                }
                RuntimeState::Paused => {
                    self.notify.notified().await;
                    continue;
                }
                RuntimeState::Cancelled => return Err("Execution cancelled by user"),
            }
        }
    }

    pub async fn pause(&self) {
        let mut state = self.state.write().await;
        if *state == RuntimeState::Running || *state == RuntimeState::Idle {
            *state = RuntimeState::Paused;
            info!("Execution paused");
        }
    }

    /// Transition to `Running` and wake waiters.
    pub async fn resume(&self) {
        let mut state = self.state.write().await;
        if *state == RuntimeState::Paused || *state == RuntimeState::Stepping {
            *state = RuntimeState::Running;
            drop(state);
            self.notify.notify_waiters();
            info!("Execution resumed");
        }
    }

    /// Transition to `Stepping` and wake waiters.
    pub async fn step(&self) {
        let mut state = self.state.write().await;
        *state = RuntimeState::Stepping;
        drop(state);
        self.notify.notify_waiters();
        info!("Single step triggered");
    }

    /// Transition to `Cancelled` and wake waiters.
    pub async fn cancel(&self) {
        let mut state = self.state.write().await;
        *state = RuntimeState::Cancelled;
        drop(state);
        self.notify.notify_waiters();
        info!("Execution cancelled");
    }

    /// Reset to `Idle` and wake any waiters so a new execution can start.
    pub async fn reset(&self) {
        let mut state = self.state.write().await;
        *state = RuntimeState::Idle;
        // A steer belongs to the turn that was running; a new one starts clean.
        let _ = self.drain_steers();
        self.iteration
            .store(0, std::sync::atomic::Ordering::Relaxed);
        drop(state);
        self.notify.notify_waiters();
    }

    /// Drop a message into the running turn.
    ///
    /// Steer is not cancel-and-resend: the work already done stays, and the
    /// next iteration sees the new instruction appended to the conversation.
    /// The tool-call loop drains this between iterations, so the message lands
    /// where the model can act on it and nowhere earlier.
    pub fn steer(&self, message: impl Into<String>) {
        let mut pending = self.steers.lock().unwrap_or_else(|e| e.into_inner());
        pending.push(message.into());
        // A turn parked in `Paused` was waiting for a human; a steer is that
        // human saying something, and leaving it parked would drop the message
        // on the floor. (Only pause is lifted; a cancel stays cancelled.)
        drop(pending);
        self.notify.notify_waiters();
        info!("Execution steered");
    }

    /// Take the steers queued since the last call.
    pub fn drain_steers(&self) -> Vec<String> {
        let mut pending = self.steers.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *pending)
    }

    /// Current runtime state.
    pub async fn current_state(&self) -> RuntimeState {
        *self.state.read().await
    }

    /// Current iteration count (number of times check_and_wait has allowed
    /// execution to proceed).
    pub fn current_iteration(&self) -> usize {
        self.iteration.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execution_controller_running() {
        let ctrl = ExecutionController::new();
        ctrl.reset().await;
        // Running / Idle -> returns immediately
        assert!(ctrl.check_and_wait().await.is_ok());
    }

    #[tokio::test]
    async fn test_execution_controller_cancel() {
        let ctrl = ExecutionController::new();
        ctrl.cancel().await;
        assert!(ctrl.check_and_wait().await.is_err());
    }

    #[tokio::test]
    async fn test_execution_controller_step_then_pause() {
        let ctrl = ExecutionController::new();
        ctrl.step().await;
        // First call: Stepping -> returns, then becomes Paused
        assert!(ctrl.check_and_wait().await.is_ok());
        assert_eq!(ctrl.current_state().await, RuntimeState::Paused);
    }

    #[tokio::test]
    async fn test_execution_controller_pause_resume() {
        let ctrl = ExecutionController::new();

        // Start paused
        ctrl.pause().await;

        // Spawn a task that waits
        let ctrl2 = ctrl.clone();
        let handle = tokio::spawn(async move { ctrl2.check_and_wait().await });

        // Small delay to let the task reach the wait
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Resume
        ctrl.resume().await;

        assert!(handle.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_steer_is_queued_and_taken_once() {
        let ctrl = ExecutionController::new();
        ctrl.steer("actually, use the other tab");
        assert_eq!(ctrl.drain_steers(), vec!["actually, use the other tab"]);
        // Drained means drained: the next iteration must not see it again.
        assert!(ctrl.drain_steers().is_empty());
    }

    #[tokio::test]
    async fn several_steers_keep_their_order() {
        let ctrl = ExecutionController::new();
        ctrl.steer("first");
        ctrl.steer("second");
        assert_eq!(ctrl.drain_steers(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn reset_drops_a_steer_from_the_finished_turn() {
        let ctrl = ExecutionController::new();
        ctrl.steer("belongs to the old turn");
        ctrl.reset().await;
        // A new turn starts clean; carrying it over would apply a message to
        // work it was never about.
        assert!(ctrl.drain_steers().is_empty());
    }

    #[tokio::test]
    async fn a_steer_does_not_cancel() {
        let ctrl = ExecutionController::new();
        ctrl.steer("do it differently");
        // The point of steering is that the turn survives it.
        assert_eq!(ctrl.current_state().await, RuntimeState::Idle);
        assert!(ctrl.drain_steers().len() == 1);
    }
}
