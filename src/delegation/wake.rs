//! Parent auto-wake: when a delegated child completes after its parent's turn
//! has ended, re-open the parent's session with an injected message carrying
//! the child result, so the parent continues and aggregates.
//!
//! This is the asynchronous slow path of the delegation result contract (see
//! `docs/delegation-wake.md`): `wait` handles children that finish inside a
//! synchronous window, and the `wait` timeout hands off to the wake path here.
//!
//! Design notes:
//!
//! - Notifications are buffered **per parent session** and drained in batches:
//!   one wake turn per batch, so near-simultaneous child completions produce a
//!   single woken turn that carries all results.
//! - The drain loop awaits the woken turn, then re-checks the buffer, so
//!   completions that land while a wake turn is processing are picked up by
//!   the same drain (no second concurrent turn).
//! - Concurrency is not a concern here: the [`Agent`](crate::agent::Agent)
//!   already serializes turns per conversation with a per-conversation
//!   semaphore, so a wake turn sent while the parent is mid-turn simply waits
//!   its turn.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracing::{debug, warn};

use super::DelegationTaskStore;

/// How a parent agent is woken with a child's result.
///
/// Implementations should block until the message has been delivered (for the
/// real handler this means until the woken turn ends), so the drain loop can
/// coalesce messages that arrive during the turn.
#[async_trait]
pub trait WakeHandler: Send + Sync {
    /// Deliver `message` to `parent_session`.
    async fn wake(&self, parent_session: &str, message: &str) -> crate::Result<()>;
}

/// Resolve the agent that owns a parent session.
///
/// The root of a delegation tree lives on a user session (router-bound); a
/// delegated parent lives on a `delegation:<run_id>` session (not router-bound,
/// because delegated turns run `process_message_with_progress` directly).
#[async_trait]
pub trait WakeResolver: Send + Sync {
    /// Resolve the agent that owns `session`.
    async fn resolve_agent(&self, session: &str) -> Option<Arc<crate::agent::Agent>>;
}

/// Per-parent-session buffering state.
#[derive(Default)]
struct SessionState {
    /// Pending wake messages, joined into one turn per drain batch.
    buffer: Vec<String>,
    /// Whether a drain task is already running for this session.
    draining: bool,
}

/// Coalescing dispatcher for parent wake notifications.
#[derive(Clone)]
pub struct DelegationWake {
    handler: Arc<dyn WakeHandler>,
    state: Arc<Mutex<HashMap<String, SessionState>>>,
}

impl DelegationWake {
    /// Create a new dispatcher that delivers buffered messages through
    /// `handler`.
    pub fn new(handler: Arc<dyn WakeHandler>) -> Self {
        Self {
            handler,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Buffer a wake message for `parent_session` and start a drain if one is
    /// not already running.
    ///
    /// Synchronous and lock-bounded: it only pushes to the buffer and, when it
    /// is the first notifier, spawns the drain task.  The drain runs on a
    /// detached task, so it is not subject to the 120 s tool-call ceiling, the
    /// circuit breaker, or the tracker deadlock red line.
    pub fn notify(&self, parent_session: &str, message: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.entry(parent_session.to_string()).or_default();
        entry.buffer.push(message.to_string());
        if entry.draining {
            return;
        }
        entry.draining = true;
        let session = parent_session.to_string();
        let this = self.clone();
        tokio::spawn(async move {
            this.drain(&session).await;
        });
    }

    /// Drain one session: batch-take all buffered messages, wake the parent
    /// once, and loop to pick up messages that arrived during the wake turn.
    ///
    /// The batch-take and the "buffer is empty → stop" check share one lock
    /// scope, so a `notify` either lands before the take (and is included) or
    /// after the session is removed (and starts a fresh drain) — never lost.
    async fn drain(&self, session: &str) {
        loop {
            let batch: Vec<String> = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                match state.get_mut(session) {
                    Some(s) if s.buffer.is_empty() => {
                        // Nothing pending — stop draining and clear the entry so
                        // the next notify starts a fresh drain.
                        state.remove(session);
                        return;
                    }
                    Some(s) => std::mem::take(&mut s.buffer),
                    None => return,
                }
            };
            let message = batch.join("\n\n---\n\n");
            if let Err(e) = self.handler.wake(session, &message).await {
                // A failed wake is informational: the result is still persisted
                // in the registry and task store for `delegate status` / `wait`.
                warn!("Failed to wake parent {} with delegation result: {}", session, e);
            }
        }
    }
}

/// Wake the parent agent through a detached `process_message_with_progress`
/// turn on its existing session.
pub struct AgentWakeHandler {
    resolver: Arc<dyn WakeResolver>,
}

impl AgentWakeHandler {
    /// Create a handler backed by the given session→agent resolver.
    pub fn new(resolver: Arc<dyn WakeResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl WakeHandler for AgentWakeHandler {
    async fn wake(&self, parent_session: &str, message: &str) -> crate::Result<()> {
        let agent = self
            .resolver
            .resolve_agent(parent_session)
            .await
            .ok_or_else(|| crate::error::SyscityError::NotFound {
                resource: format!("parent agent for session {}", parent_session),
            })?;

        let incoming = crate::channels::IncomingMessage::new("system", parent_session, message)
            .with_provenance(crate::channels::InputProvenance::InternalSystem {
                source: "delegation".to_string(),
            });
        let no_op: crate::agent::ProgressCallback = Arc::new(|_| Box::pin(async {}));

        // `process_message` keys history by the message's session id, so
        // passing the parent's session key resumes that conversation rather
        // than starting a fresh one.  The per-conversation semaphore inside the
        // agent serializes this turn against any turn the parent is already in.
        // The future borrows `agent`, so it is built inside the spawned task,
        // which owns the `Arc`.
        let task = async move { agent.process_message_with_progress(incoming, no_op).await };
        match tokio::spawn(task).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(join) => Err(crate::error::SyscityError::Internal(format!(
                "wake turn for session {} aborted: {}",
                parent_session, join
            ))),
        }
    }
}

/// Look up a child's parent session and buffer a wake notification for it, if
/// the parent is still active.
///
/// Called from the child's completion paths after the outcome has been written
/// to the tracker/registry/store.  When the wake is skipped (parent inactive)
/// or fails, the result remains observable via `delegate status` / `wait`.
pub async fn notify_parent(
    registry: &crate::agent::subagent_registry::SubagentRegistry,
    store: Option<&DelegationTaskStore>,
    wake: &DelegationWake,
    child_id: &str,
    message: &str,
) {
    let run = match registry.get_run(child_id).await {
        Some(run) => run,
        None => {
            warn!("Cannot wake parent for child {}: run not found", child_id);
            return;
        }
    };
    let parent_session = run.parent_session;
    if !parent_active_for_wake(store, &parent_session).await {
        debug!(
            "Skipping wake for child {}: parent {} is no longer active",
            child_id, parent_session
        );
        return;
    }
    wake.notify(&parent_session, message);
}

/// Whether a parent session should still be woken when a child completes.
///
/// - Root parents (no `delegation:` prefix, no task row) are always woken —
///   their turn may already have ended with the child still outstanding.
/// - Delegated parents are woken unless their own task row is already terminal
///   (`completed`, `failed`, `waiting_handoff`), in which case a wake would be
///   noise: the parent has finished its part, or the task has been handed off
///   to a successor that owns continuation.
/// - Missing/errored rows default to active (wake), matching the "no store →
///   wake" fallback.
pub async fn parent_active_for_wake(
    store: Option<&DelegationTaskStore>,
    parent_session: &str,
) -> bool {
    let Some(run_id) = parent_session.strip_prefix("delegation:") else {
        return true; // root parent: not a delegation task row
    };
    let Some(store) = store else {
        return true; // no store attached — cannot check, assume active
    };
    match store.get_task(run_id).await {
        Ok(Some(task)) => {
            !matches!(task.status.as_str(), "completed" | "failed" | "waiting_handoff")
        }
        _ => true,
    }
}

/// The wake message delivered to a parent when a child completes.
pub fn child_completion_message(child_id: &str, result: &str) -> String {
    format!(
        "Your delegated child {child_id} completed.\n\nResult:\n{result}\n\n\
         Summarize or aggregate this result into your current task, then continue. \
         If all your delegated children are done, produce your final answer."
    )
}

/// The wake message delivered to a parent when a child fails.
pub fn child_failure_message(child_id: &str, error: &str) -> String {
    format!(
        "Your delegated child {child_id} failed.\n\nError:\n{error}\n\n\
         Decide whether to retry, re-delegate, or proceed with the partial results \
         you already have."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Immediate handler that records every wake call.
    struct RecordingHandler {
        wakes: Arc<Mutex<Vec<(String, String)>>>,
        calls: Arc<AtomicUsize>,
    }

    impl RecordingHandler {
        fn new() -> Self {
            Self {
                wakes: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl WakeHandler for RecordingHandler {
        async fn wake(&self, parent_session: &str, message: &str) -> crate::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.wakes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((parent_session.to_string(), message.to_string()));
            Ok(())
        }
    }

    /// Handler that records every wake call and, when `hold` is set, blocks on
    /// a semaphore so a test can control drain timing deterministically.
    struct GatedHandler {
        wakes: Arc<Mutex<Vec<(String, String)>>>,
        hold: bool,
        gate: Arc<tokio::sync::Semaphore>,
    }

    impl GatedHandler {
        fn new(hold: bool) -> Self {
            Self {
                wakes: Arc::new(Mutex::new(Vec::new())),
                hold,
                gate: Arc::new(tokio::sync::Semaphore::new(0)),
            }
        }
    }

    #[async_trait]
    impl WakeHandler for GatedHandler {
        async fn wake(&self, parent_session: &str, message: &str) -> crate::Result<()> {
            self.wakes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((parent_session.to_string(), message.to_string()));
            if self.hold {
                self.gate
                    .acquire()
                    .await
                    .expect("gate semaphore closed")
                    .forget();
            }
            Ok(())
        }
    }

    async fn wait_until(
        wakes: &Arc<Mutex<Vec<(String, String)>>>,
        len: usize,
        timeout: std::time::Duration,
    ) {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if wakes.lock().unwrap_or_else(|e| e.into_inner()).len() >= len {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        let seen = wakes.lock().unwrap_or_else(|e| e.into_inner()).len();
        panic!("timed out waiting for {len} wake(s), saw {seen}");
    }

    #[tokio::test]
    async fn test_single_notify_wakes_once() {
        let handler = Arc::new(RecordingHandler::new());
        let wake = DelegationWake::new(handler.clone());
        wake.notify("session-1", "result A");
        wait_until(&handler.wakes, 1, std::time::Duration::from_secs(2)).await;
        let wakes = handler.wakes.lock().unwrap().clone();
        assert_eq!(wakes.len(), 1);
        assert_eq!(wakes[0].0, "session-1");
        assert!(wakes[0].1.contains("result A"));
    }

    #[tokio::test]
    async fn test_two_notifies_coalesce_into_one_wake() {
        let handler = Arc::new(RecordingHandler::new());
        let wake = DelegationWake::new(handler.clone());
        wake.notify("session-1", "result A");
        wake.notify("session-1", "result B");
        wait_until(&handler.wakes, 1, std::time::Duration::from_secs(2)).await;
        let wakes = handler.wakes.lock().unwrap().clone();
        assert_eq!(wakes.len(), 1, "two notifies must coalesce into one wake");
        assert!(wakes[0].1.contains("result A"));
        assert!(wakes[0].1.contains("result B"));
    }

    #[tokio::test]
    async fn test_different_sessions_wake_independently() {
        let handler = Arc::new(RecordingHandler::new());
        let wake = DelegationWake::new(handler.clone());
        wake.notify("session-1", "A");
        wake.notify("session-2", "B");
        wait_until(&handler.wakes, 2, std::time::Duration::from_secs(2)).await;
        let wakes = handler.wakes.lock().unwrap().clone();
        assert_eq!(wakes.len(), 2);
    }

    #[tokio::test]
    async fn test_messages_landing_mid_turn_are_picked_up() {
        // A completion that lands while the first wake turn is still running is
        // delivered by the same drain loop's next iteration — a second wake
        // call, never a concurrent second turn.
        let handler = Arc::new(GatedHandler::new(true));
        let wake = DelegationWake::new(handler.clone());
        wake.notify("session-1", "first");
        wait_until(&handler.wakes, 1, std::time::Duration::from_secs(2)).await;
        // First wake is blocked on the gate; buffer the second completion.
        wake.notify("session-1", "second");
        handler.gate.add_permits(1); // release first wake → drain loops
        handler.gate.add_permits(1); // release second wake → drain ends
        wait_until(&handler.wakes, 2, std::time::Duration::from_secs(2)).await;
        let wakes = handler.wakes.lock().unwrap().clone();
        assert_eq!(wakes.len(), 2);
        assert!(wakes[1].1.contains("second"));
    }

    #[test]
    fn test_completion_message_mentions_child_and_result() {
        let msg = child_completion_message("child-1", "the answer");
        assert!(msg.contains("child-1"));
        assert!(msg.contains("the answer"));
        assert!(msg.contains("aggregate"));
    }

    #[test]
    fn test_failure_message_mentions_child_and_error() {
        let msg = child_failure_message("child-2", "boom");
        assert!(msg.contains("child-2"));
        assert!(msg.contains("boom"));
    }

    #[tokio::test]
    async fn test_parent_active_for_wake_root_always_wakes() {
        let store = Arc::new(
            DelegationTaskStore::new("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        // Root session: no delegation: prefix → always active.
        assert!(parent_active_for_wake(Some(&store), "user-session-1").await);
        // No store → assume active.
        assert!(parent_active_for_wake(None, "delegation:run-1").await);
    }

    #[tokio::test]
    async fn test_parent_active_for_wake_delegated_parent() {
        use super::super::NewTask;
        let store = Arc::new(
            DelegationTaskStore::new("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        for id in ["run-running", "run-done", "run-failed", "run-handoff"] {
            store
                .create_task(NewTask {
                    id,
                    root_id: "root-1",
                    parent_id: None,
                    depth: 1,
                    agent_id: "manager",
                    title: "T",
                    parent_session: None,
                })
                .await
                .unwrap();
        }
        store.set_status("run-done", "completed").await.unwrap();
        store.set_status("run-failed", "failed").await.unwrap();
        store
            .set_handoff("run-handoff", "reviewer", "needs review")
            .await
            .unwrap();

        assert!(parent_active_for_wake(Some(&store), "delegation:run-running").await);
        assert!(!parent_active_for_wake(Some(&store), "delegation:run-done").await);
        assert!(!parent_active_for_wake(Some(&store), "delegation:run-failed").await);
        assert!(!parent_active_for_wake(Some(&store), "delegation:run-handoff").await);
        // Unknown run id → default to active.
        assert!(parent_active_for_wake(Some(&store), "delegation:ghost").await);
    }
}
