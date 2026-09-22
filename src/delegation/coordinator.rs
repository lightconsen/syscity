//! Handoff coordination for delegation trees.
//!
//! A delegated agent can hand its task to a successor agent via the
//! `task_state` tool's `handoff` action, which marks the task
//! `waiting_handoff` and names a target agent.  When a child task finishes
//! (successfully or not), [`DelegationCoordinator::maybe_advance`] looks for
//! any pending handoff under the same tree root and spawns a successor that
//! continues the work.
//!
//! The successor is a new delegation task whose parent row is the
//! handing-off task.  It inherits the same tree root and depth — a
//! continuation of the same task, not a deeper recursion — so handoff chains
//! never consume the delegation depth budget.
//!
//! `maybe_advance` is driven synchronously by child completion (see
//! `DelegateTool`), not by a background loop, so it shares the gateway's
//! lifecycle and shutdown story with no extra task lifetimes.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::warn;

use super::{DelegationScope, DelegationTask, DelegationTaskStore};
use crate::agent::subagent_registry::SubagentRegistry;
use crate::agent::Agent;
use crate::tools::delegate_tool::{
    execute_child_task, AgentResolver, ChildTaskEnv, DelegationTracker, TaskSpec,
};

/// Resolves handoff targets and spawns their successor tasks.
///
/// Wraps the shared task store, the shared [`SubagentRegistry`] (so successor
/// runs count against the same concurrency/depth limits as ordinary
/// delegations), and the agent resolver for routing `handoff` targets.
#[derive(Clone)]
pub struct DelegationCoordinator {
    store: Arc<DelegationTaskStore>,
    registry: Arc<SubagentRegistry>,
    resolver: Arc<dyn AgentResolver>,
    default_agent: Option<Arc<Agent>>,
    /// Serializes handoff advancement so two children finishing concurrently
    /// cannot both pick up the same pending handoff and spawn duplicate
    /// successors.
    advance_lock: Arc<Mutex<()>>,
}

impl DelegationCoordinator {
    /// Create a coordinator over the given store and registry.
    ///
    /// `resolver` routes `handoff <to_agent>` targets; `default_agent` is the
    /// fallback when the named target cannot be resolved.
    pub fn new(
        store: Arc<DelegationTaskStore>,
        registry: Arc<SubagentRegistry>,
        resolver: Arc<dyn AgentResolver>,
        default_agent: Option<Arc<Agent>>,
    ) -> Self {
        Self {
            store,
            registry,
            resolver,
            default_agent,
            advance_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl std::fmt::Debug for DelegationCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegationCoordinator")
            .field("has_default_agent", &self.default_agent.is_some())
            .finish()
    }
}

impl DelegationCoordinator {
    /// Advance the delegation tree at `root_id` if any task under it is
    /// waiting for a handoff successor.
    ///
    /// Resolves the target agent, spawns a successor task via the registry,
    /// and returns the successor's run id.  Returns `Ok(None)` when there is
    /// no pending handoff, or when the successor cannot be started (the
    /// handing-off task is then marked failed so the tree does not retry
    /// forever).
    pub async fn maybe_advance(&self, root_id: &str) -> crate::Result<Option<String>> {
        // Serialize so concurrent child completions cannot double-spawn a
        // successor for the same pending handoff.
        let _guard = self.advance_lock.lock().await;
        self.maybe_advance_inner(root_id).await
    }

    async fn maybe_advance_inner(&self, root_id: &str) -> crate::Result<Option<String>> {
        let Some(handoff) = self.store.pending_handoff_for_root(root_id).await? else {
            return Ok(None);
        };

        let target = handoff.agent_id.clone();
        let agent = self
            .resolver
            .resolve(&target)
            .await
            .or_else(|| self.default_agent.clone());

        let Some(agent) = agent else {
            warn!(
                task = %handoff.id,
                target = %target,
                "Handoff successor agent unavailable; marking task failed"
            );
            if let Err(e) = self.store.set_status(&handoff.id, "failed").await {
                warn!("Failed to mark unavailable handoff task '{}' failed: {}", handoff.id, e);
            }
            return Ok(None);
        };

        match self.spawn_successor(&handoff, agent).await {
            Ok(successor_id) => {
                // The handing-off task is consumed by this successor; mark it
                // completed so later advances do not re-pick it up.
                if let Err(e) = self.store.set_status(&handoff.id, "completed").await {
                    warn!("Failed to mark handed-off task '{}' completed: {}", handoff.id, e);
                }
                Ok(Some(successor_id))
            }
            Err(e) => {
                warn!(
                    task = %handoff.id,
                    error = %e,
                    "Failed to spawn handoff successor; marking task failed"
                );
                if let Err(se) = self.store.set_status(&handoff.id, "failed").await {
                    warn!("Failed to mark failed handoff task '{}' failed: {}", handoff.id, se);
                }
                Ok(None)
            }
        }
    }

    /// Spawn a successor task continuing `handoff`'s work on `agent`.
    ///
    /// The successor reuses the handoff's root and depth, links its parent row
    /// to the handing-off task, and is prompted with the handoff summary plus
    /// the current shared state and artifact list so it can pick up where the
    /// predecessor left off.
    async fn spawn_successor(
        &self,
        handoff: &DelegationTask,
        agent: Arc<Agent>,
    ) -> crate::Result<String> {
        let target = handoff.agent_id.clone();
        let prompt = successor_prompt(handoff);
        let parent_session = handoff.id.clone();
        let scope_base = DelegationScope {
            root_id: handoff.root_id.clone(),
            task_id: String::new(), // filled with the registry run id below
            parent_task_id: Some(handoff.id.clone()),
            depth: handoff.depth,
            max_depth: self.registry.max_depth(),
            allowed_tools: None,
            max_iterations: None,
        };

        let spec = TaskSpec {
            prompt: prompt.clone(),
            output_format: None,
            max_iterations: None,
            allowed_tools: vec![],
            context: HashMap::new(),
            target_agent: Some(target.clone()),
            task_id: None,
        };
        let tracker = DelegationTracker::new(handoff.depth as usize);
        let iterations = Arc::new(AtomicUsize::new(0));
        let store = self.store.clone();
        let registry = self.registry.clone();
        let resolver = self.resolver.clone();
        let default_agent = self.default_agent.clone();
        // Cloned outside the spawn closure: the closure outlives this method
        // and must not borrow `handoff`.
        let successor_parent_session = handoff.parent_session.clone();

        let run_id = self
            .registry
            .spawn(&parent_session, &target, &prompt, handoff.depth, {
                let spec = spec.clone();
                let tracker = tracker.clone();
                let iterations = iterations.clone();
                let agent = agent.clone();
                let registry = Arc::clone(&registry);
                let store = Arc::clone(&store);
                let resolver = Arc::clone(&resolver);
                let default_agent = default_agent.clone();
                let target = target.clone();
                let successor_parent_session = successor_parent_session.clone();
                move |run_id, _task_str| {
                    let mut scope = scope_base.clone();
                    scope.task_id = run_id.clone();
                    // A coordinator over the same shared store/registry, so the
                    // successor's own completion advances further handoffs
                    // (chains of continuation).  Built inside the closure to
                    // keep the coordinator type out of the closure captures.
                    let coordinator = Arc::new(DelegationCoordinator::new(
                        store.clone(),
                        registry.clone(),
                        resolver,
                        default_agent,
                    ));
                    Box::pin(async move {
                        execute_child_task(
                            run_id,
                            spec,
                            // No wake: a successor's parent session is the
                            // handing-off task's id, not a live agent session,
                            // so waking it would be spurious.
                            ChildTaskEnv {
                                tracker,
                                iterations,
                                agent: Some(agent),
                                registry,
                                store: Some(store),
                                scope,
                                agent_id: target,
                                coordinator: Some(coordinator),
                                wake: None,
                                // Lineage for push events: the successor
                                // inherits the handing-off task's session, so
                                // its row resolves to the same root user
                                // session (the registry's own parent key above
                                // is wake-keying only and must not be recorded).
                                parent_session: successor_parent_session,
                            },
                        )
                        .await;
                    })
                }
            })
            .await?;

        Ok(run_id)
    }
}

/// Build the successor's prompt from the handoff record: the summary plus the
/// current shared state and artifacts so it can continue seamlessly.
fn successor_prompt(handoff: &DelegationTask) -> String {
    let state_blob = serde_json::from_str::<serde_json::Value>(&handoff.state_json)
        .unwrap_or(serde_json::Value::Null);
    let state_text = serde_json::to_string_pretty(&state_blob).unwrap_or_default();
    let artifacts: Vec<String> = handoff
        .artifacts
        .iter()
        .map(|a| format!("- {}: {}", a.name, a.url))
        .collect();
    let artifact_text = if artifacts.is_empty() {
        "None".to_string()
    } else {
        artifacts.join("\n")
    };

    format!(
        "A previous agent handed this task off to you. Continue it and complete it.\n\n\
         Task: {}\n\n\
         Handoff summary: {}\n\n\
         Shared state so far:\n{}\n\n\
         Artifacts produced so far:\n{}\n\n\
         Use the `task_state` tool to read and update the shared state as you work.",
        handoff.title,
        handoff_summary(handoff),
        state_text,
        artifact_text,
    )
}

/// Extract the most recent `handoff` event's detail as the summary text.
fn handoff_summary(task: &DelegationTask) -> String {
    task.events
        .iter()
        .rev()
        .find(|e| e.action == "handoff")
        .map(|e| e.detail.clone())
        .unwrap_or_else(|| "Continue the original task".to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::delegation::NewTask;
    use crate::providers::mock::MockProvider;
    use crate::tools::ToolRegistry;

    fn mock_agent() -> Arc<Agent> {
        let provider = Arc::new(
            MockProvider::new().with_responses(vec![crate::providers::Message::assistant("done")]),
        );
        Arc::new(Agent::new(AgentConfig::default(), provider, Arc::new(ToolRegistry::new())))
    }

    struct FakeResolver {
        agent: Option<Arc<Agent>>,
    }

    #[async_trait]
    impl AgentResolver for FakeResolver {
        async fn resolve(&self, _name: &str) -> Option<Arc<Agent>> {
            self.agent.clone()
        }
    }

    async fn setup(
        resolver: Arc<dyn AgentResolver>,
    ) -> (Arc<DelegationTaskStore>, Arc<SubagentRegistry>, Arc<DelegationCoordinator>) {
        let store = Arc::new(
            DelegationTaskStore::new("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        let registry = Arc::new(SubagentRegistry::new(3, 10));
        let coordinator =
            Arc::new(DelegationCoordinator::new(store.clone(), registry.clone(), resolver, None));
        (store, registry, coordinator)
    }

    /// Wait for the background successor task to create its row.
    async fn wait_for_task(store: &DelegationTaskStore, id: &str) -> DelegationTask {
        for _ in 0..100 {
            if let Some(task) = store.get_task(id).await.expect("read task") {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {} was never created", id);
    }

    #[tokio::test]
    async fn test_no_pending_handoff_is_noop() {
        let agent = mock_agent();
        let resolver: Arc<dyn AgentResolver> = Arc::new(FakeResolver { agent: Some(agent) });
        let (store, _registry, coordinator) = setup(resolver).await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "T",
                parent_session: None,
            })
            .await
            .unwrap();

        assert!(coordinator.maybe_advance("root-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_unresolvable_target_marks_task_failed() {
        let resolver: Arc<dyn AgentResolver> = Arc::new(FakeResolver { agent: None });
        let (store, _registry, coordinator) = setup(resolver).await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "T",
                parent_session: None,
            })
            .await
            .unwrap();
        store
            .set_handoff("run-1", "ghost", "please continue")
            .await
            .unwrap();

        assert!(coordinator.maybe_advance("root-1").await.unwrap().is_none());

        let task = store.get_task("run-1").await.unwrap().unwrap();
        assert_eq!(task.status, "failed");
    }

    #[tokio::test]
    async fn test_advances_pending_handoff_to_successor() {
        let agent = mock_agent();
        let resolver: Arc<dyn AgentResolver> = Arc::new(FakeResolver { agent: Some(agent) });
        let (store, registry, coordinator) = setup(resolver).await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "Original task",
                parent_session: None,
            })
            .await
            .unwrap();
        store
            .set_handoff("run-1", "worker", "finish the parser")
            .await
            .unwrap();

        let successor = coordinator
            .maybe_advance("root-1")
            .await
            .unwrap()
            .expect("successor spawned");

        // Successor row exists under the same root, linked to the handoff task,
        // continuing at the same depth.
        let succ = wait_for_task(&store, &successor).await;
        assert_eq!(succ.parent_id.as_deref(), Some("run-1"));
        assert_eq!(succ.depth, 1);
        assert_eq!(succ.agent_id, "worker");

        // The handing-off task was consumed (no longer waiting_handoff), so a
        // second advance is a no-op.
        let run1 = store.get_task("run-1").await.unwrap().unwrap();
        assert_eq!(run1.status, "completed");
        assert!(coordinator.maybe_advance("root-1").await.unwrap().is_none());

        // Registry recorded the successor run targeting the handoff agent.
        let runs = registry.runs_for_session("run-1").await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].target_agent, "worker");
    }

    /// The successor's row inherits the handing-off task's `parent_session`,
    /// so its events resolve to the same root user session. The registry's
    /// own parent key (the handing-off task's id) is wake-keying only and
    /// must not be recorded as lineage.
    #[tokio::test]
    async fn test_successor_copies_the_handoff_row_parent_session() {
        let agent = mock_agent();
        let resolver: Arc<dyn AgentResolver> = Arc::new(FakeResolver { agent: Some(agent) });
        let (store, _registry, coordinator) = setup(resolver).await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "Original task",
                parent_session: Some("user-session"),
            })
            .await
            .unwrap();
        store
            .set_handoff("run-1", "worker", "finish the parser")
            .await
            .unwrap();

        let successor = coordinator
            .maybe_advance("root-1")
            .await
            .unwrap()
            .expect("successor spawned");

        let succ = wait_for_task(&store, &successor).await;
        assert_eq!(
            succ.parent_session.as_deref(),
            Some("user-session"),
            "lineage rides the handing-off row, not the registry's wake key"
        );
        // No `usage_tokens` assertion: `maybe_advance` spawns the successor, so this
        // counter is written concurrently by the child's own progress callback
        // (`add_usage`) while `wait_for_task` only waits for the row to *exist*.
        // Asserting it is zero asserted that the child had not finished its first
        // round yet — a property of the scheduler, not of this code.
        // And it resolves through the same chain the forwarder uses.
        assert_eq!(
            store.root_session_for_task(&succ.id).await.unwrap(),
            Some("user-session".to_string())
        );
    }
}
