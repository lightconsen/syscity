//! Shared goal-runner lifecycle helpers.
//!
//! A goal is *suspended* when it has a persisted checkpoint but no live
//! runner — the state after a gateway restart (startup deliberately does not
//! re-arm autonomous loops) or after a policy stop (loop detected / max
//! rounds, which keep their checkpoint). `spawn_goal_runner` is the single
//! place that turns a persisted checkpoint back into a running goal; today
//! its only caller is `/goal resume`.

use std::sync::Arc;

use tracing::warn;

use super::GatewayState;
use crate::goal::persist::PersistedGoalState;

/// A persisted goal with no live runner.
#[derive(Debug)]
pub(crate) struct SuspendedGoal {
    pub goal_id: String,
    pub round: usize,
    pub max_rounds: usize,
    pub blocked_reason: Option<crate::goal::BlockedReason>,
}

/// List goals that are persisted but not running (suspended).
pub(crate) async fn list_suspended(state: &Arc<GatewayState>) -> Vec<SuspendedGoal> {
    let store = crate::goal::persist::GoalStore::new();
    let persisted = store.load_all().await;
    let cancellers = state.agents.goal_cancellers.read().await;
    persisted
        .iter()
        .filter(|p| !cancellers.contains_key(&p.goal_id))
        .map(|p| SuspendedGoal {
            goal_id: p.goal_id.clone(),
            round: p.round,
            max_rounds: p.plan.max_rounds,
            blocked_reason: p.blocked_reason.clone(),
        })
        .collect()
}

/// Write a goal's terminal outcome into the parent session thread so the
/// chat history keeps a durable record even after the live WS events are
/// gone. Best-effort: a failure only warns and never affects the goal.
async fn write_goal_outcome_to_session(
    state: &Arc<GatewayState>,
    session_id: &str,
    goal_id: &str,
    event: &crate::goal::GoalEvent,
) {
    let tokens = |u: &Option<crate::agent::turns::TurnUsage>| {
        u.map(|t| format!(" Tokens: {}.", t.total_tokens))
            .unwrap_or_default()
    };
    let text = match event {
        crate::goal::GoalEvent::Done {
            total_rounds,
            summary,
            token_usage,
            ..
        } => format!(
            "🏁 Goal `{}` completed in {} round(s).{} {}",
            goal_id,
            total_rounds,
            tokens(token_usage),
            summary
        ),
        crate::goal::GoalEvent::Aborted {
            reason,
            round,
            token_usage,
            ..
        } => format!(
            "⛔ Goal `{}` aborted at round {}:{}.{} Check `/goal list`; if suspended, resume with `/goal resume {}`.",
            goal_id,
            round,
            reason,
            tokens(token_usage),
            goal_id
        ),
        _ => return,
    };
    if let Some(ref store) = state.agents.store {
        if let Err(e) = store
            .append_message(&crate::agent::session_store::AppendMessageParams {
                session_id,
                role: "assistant",
                content: &text,
                ..Default::default()
            })
            .await
        {
            warn!("[goal {}] Failed to write outcome to parent session: {}", goal_id, e);
        }
    }
}

/// Spawn the GoalEvent → GatewayEvent relay for a goal.
///
/// Registered as `goal-relay:{id}` in the TaskRegistry so gateway shutdown
/// drains it alongside the runner itself. Terminal outcomes are additionally
/// written back into the parent session thread.
pub(crate) async fn spawn_goal_relay(
    state: &Arc<GatewayState>,
    goal_id: &str,
    session_id: String,
    goal_rx: tokio::sync::mpsc::UnboundedReceiver<crate::goal::GoalEvent>,
) {
    let event_tx = state.events.tx.clone();
    let gid = goal_id.to_string();
    let state_for_outcome = state.clone();
    let handle = tokio::spawn(async move {
        let mut goal_rx = goal_rx;
        while let Some(goal_event) = goal_rx.recv().await {
            // Durable record first: even if the live broadcast fails, the
            // parent session keeps the outcome.
            let is_terminal = matches!(
                goal_event,
                crate::goal::GoalEvent::Done { .. } | crate::goal::GoalEvent::Aborted { .. }
            );
            if is_terminal {
                write_goal_outcome_to_session(&state_for_outcome, &session_id, &gid, &goal_event)
                    .await;
            }
            let gw_event = crate::gateway::GatewayEvent::GoalProgress {
                goal_id: gid.clone(),
                session_id: session_id.clone(),
                event: goal_event,
            };
            if let Err(e) = event_tx.send(gw_event) {
                warn!("[goal] Failed to broadcast event: {}", e);
                break;
            }
        }
    });
    state
        .task_registry
        .insert_join(format!("goal-relay:{goal_id}"), handle)
        .await;
}

/// Spawn a goal runner as a registered background task (`goal:{id}`).
///
/// Registration matters for two reasons: gateway shutdown aborts registered
/// tasks (crash-equivalent — the last round checkpoint survives and the goal
/// becomes suspended), and it satisfies the project rule that every spawned
/// task is tracked. The cancellers entry is removed when the runner exits on
/// its own; shutdown cleanup relies on the drain in `stop_gateway`.
pub(crate) async fn spawn_registered_runner(
    state: &Arc<GatewayState>,
    goal_id: &str,
    runner: crate::goal::GoalRunner,
) {
    let task_name = format!("goal:{goal_id}");
    let gid = goal_id.to_string();
    let cleanup_name = task_name.clone();
    let cancellers = state.agents.goal_cancellers.clone();
    let registry = state.task_registry.clone();
    let handle = tokio::spawn(async move {
        runner.run().await;
        // Clean up cancellers entry on completion.
        let mut c = cancellers.write().await;
        c.remove(&gid);
        drop(c);
        // Drop our own registry entry so finished goals don't linger.
        registry.remove_matching_join_or_abort(&cleanup_name).await;
    });
    state.task_registry.insert_join(task_name, handle).await;
}

/// Spawn a goal runner from a persisted checkpoint: event relay, canceller
/// registration, and the runner task. Returns the goal id.
pub(crate) async fn spawn_goal_runner(
    state: &Arc<GatewayState>,
    persisted: &PersistedGoalState,
) -> String {
    spawn_goal_runner_with_store(state, persisted, Some(crate::goal::persist::shared_store())).await
}

/// Like [`spawn_goal_runner`] but with an injectable checkpoint store
/// (tests use a temp directory instead of the real `~/.syscity/goals`).
pub(crate) async fn spawn_goal_runner_with_store(
    state: &Arc<GatewayState>,
    persisted: &PersistedGoalState,
    store: Option<crate::goal::persist::SharedGoalStore>,
) -> String {
    let goal_id = persisted.goal_id.clone();
    let (goal_tx, goal_rx) = tokio::sync::mpsc::unbounded_channel();

    // Event relay: GoalEvent → GatewayEvent.
    spawn_goal_relay(state, &goal_id, persisted.parent_session_id.clone(), goal_rx).await;

    let (goal_id, parent_sid, plan, condition_history, token_usage) =
        crate::goal::persist::to_runner_params(persisted);

    let mut runner = crate::goal::GoalRunner::new(
        &goal_id,
        &parent_sid,
        plan,
        state.tools.registry.clone(),
        state.infra.model_router.clone(),
        goal_tx,
        state.paths.clone(),
    )
    .with_progress(persisted.round, condition_history)
    // Fresh-context goals resume with the same carried handoff they had
    // in-process — a restart must not silently drop the only inter-round
    // state.
    .with_handoff(persisted.last_handoff.clone())
    // Mid-round resume: continue the interrupted round from its restored
    // message history instead of restarting the round from scratch.
    .with_round_messages(persisted.round_messages.clone())
    // Cost axis: keep counting from the persisted cumulative total.
    .with_token_usage(token_usage);
    if let Some(store) = store {
        runner = runner.with_store(store);
    }

    let cancel_token = runner.cancel_token();
    {
        let mut cancellers = state.agents.goal_cancellers.write().await;
        cancellers.insert(goal_id.clone(), cancel_token);
    }

    spawn_registered_runner(state, &goal_id, runner).await;

    goal_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::goal::condition::GoalCondition;
    use crate::model_router::ModelRouterConfig;
    use crate::providers::mock::MockProvider;

    async fn make_mock_router(mock: MockProvider) -> Arc<crate::model_router::ModelRouter> {
        let router = crate::model_router::ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider_instance("mock", Arc::new(mock))
            .await
            .unwrap();
        router
            .model_catalog
            .register(crate::model_router::model_catalog::ModelCatalogEntry::new(
                "test-model",
                "test-model",
                "mock",
            ))
            .await;
        Arc::new(router)
    }

    /// A suspended goal whose runner would stay busy for a while: the mock
    /// router answers immediately, but the round's exit-code check sleeps.
    fn persisted_sample(goal_id: &str, session_id: &str) -> PersistedGoalState {
        PersistedGoalState {
            goal_id: goal_id.to_string(),
            parent_session_id: session_id.to_string(),
            plan: crate::goal::GoalPlan::new("keep busy")
                .with_condition(GoalCondition::ExitCode {
                    command: "sleep 2 && exit 1".to_string(),
                    expected: Some(0),
                })
                .with_max_rounds(5)
                .with_model("test-model"),
            round: 0,
            condition_history: vec![],
            blocked_reason: None,
            last_handoff: None,
            token_usage: None,
            round_messages: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn spawn_registers_runner_and_relay_for_shutdown_drain() {
        let mut state =
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await;
        state.infra.model_router = make_mock_router(MockProvider::new()).await;
        let state = Arc::new(state);

        let dir = std::env::temp_dir().join(format!("goal_spawn_{}", uuid::Uuid::new_v4()));
        let store = Arc::new(tokio::sync::RwLock::new(crate::goal::persist::GoalStore::with_dir(
            dir.clone(),
        )));

        let goal_id = spawn_goal_runner_with_store(
            &state,
            &persisted_sample("goal_reg", "sess_reg"),
            Some(store),
        )
        .await;
        assert_eq!(goal_id, "goal_reg");

        // Both the runner (`goal:<id>`) and the event relay
        // (`goal-relay:<id>`) must be registered so shutdown drains them.
        let handles = state
            .task_registry
            .remove_matching_join_or_abort("goal")
            .await;
        assert!(handles.len() >= 2, "expected runner + relay registered, got {}", handles.len());
        for h in handles {
            h.abort();
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn write_back_appends_terminal_outcome_to_session() {
        let state = Arc::new(
            crate::gateway::state_tests::make_test_state_with_store(
                crate::gateway::GatewayConfig::default(),
            )
            .await,
        );
        let store = state
            .agents
            .store
            .as_ref()
            .expect("test state has a session store");

        write_goal_outcome_to_session(
            &state,
            "sess_wb",
            "goal_wb",
            &crate::goal::GoalEvent::Done {
                total_rounds: 3,
                all_passed: true,
                summary: "all checks green".to_string(),
                token_usage: Some(crate::agent::turns::TurnUsage {
                    prompt_tokens: 100,
                    completion_tokens: 20,
                    total_tokens: 120,
                    ..Default::default()
                }),
            },
        )
        .await;

        write_goal_outcome_to_session(
            &state,
            "sess_wb",
            "goal_wb",
            &crate::goal::GoalEvent::Aborted {
                reason: "round budget exhausted".to_string(),
                round: 5,
                results: vec![],
                blocked_reason: Some(crate::goal::BlockedReason {
                    code: crate::goal::BlockedReasonCode::MaxRounds,
                    message: "round budget exhausted".to_string(),
                }),
                token_usage: None,
            },
        )
        .await;

        // Non-terminal events must never write back.
        write_goal_outcome_to_session(
            &state,
            "sess_wb",
            "goal_wb",
            &crate::goal::GoalEvent::Check {
                round: 1,
                results: vec![],
                passed: 0,
                total: 1,
            },
        )
        .await;

        let messages = store.get_messages("sess_wb", 10, None).await.unwrap();
        assert_eq!(messages.len(), 2, "only terminal events write back");
        // Both messages land within the same millisecond, so ordering between
        // them is not guaranteed — assert by content, not position.
        for (_, role, content, ..) in &messages {
            assert_eq!(role, "assistant");
        }
        let aborted = messages
            .iter()
            .find(|m| m.2.contains("aborted at round 5"))
            .expect("aborted outcome written back");
        assert!(aborted.2.contains("/goal resume goal_wb"));
        let done = messages
            .iter()
            .find(|m| m.2.contains("completed in 3 round(s)"))
            .expect("done outcome written back");
        assert!(done.2.contains("Tokens: 120."));
    }

    #[tokio::test]
    async fn spawn_resumes_midround_messages_end_to_end() {
        use crate::providers::{FunctionCall, Message, Role, ToolCall};

        let mut state =
            crate::gateway::state_tests::make_test_state(crate::gateway::GatewayConfig::default())
                .await;
        // The callback distinguishes a mid-round resume (restored prefix has
        // >2 messages) from a fresh round rebuild (system+user only).
        let mock = MockProvider::new().with_callback(|messages| {
            if messages.len() > 2 {
                Message::assistant("resumed final")
            } else {
                Message::assistant("fresh start")
            }
        });
        let mock_handle = mock.clone();
        state.infra.model_router = make_mock_router(mock).await;
        let state = Arc::new(state);

        let dir = std::env::temp_dir().join(format!("goal_spawn_{}", uuid::Uuid::new_v4()));
        let store = Arc::new(tokio::sync::RwLock::new(crate::goal::persist::GoalStore::with_dir(
            dir.clone(),
        )));

        let mut persisted = persisted_sample("goal_mid", "sess_mid");
        persisted.plan = crate::goal::GoalPlan::new("resume mid-round")
            .with_condition(GoalCondition::ExitCode {
                command: "false".to_string(),
                expected: Some(0),
            })
            .with_max_rounds(1)
            .with_model("test-model");
        persisted.round = 1;
        persisted.round_messages = Some(vec![
            Message::system("system prompt"),
            Message::user("round feedback"),
            Message::assistant("calling tool").with_tool_calls(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "some_tool".to_string(),
                    arguments: "{}".to_string(),
                },
                index: None,
                result: None,
            }]),
            Message::tool("tool output", "call_1"),
        ]);

        spawn_goal_runner_with_store(&state, &persisted, Some(store.clone())).await;

        // Wait for the resumed round's first completion.
        for _ in 0..200 {
            if mock_handle.call_count() >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let history = mock_handle.history();
        assert_eq!(history.len(), 1, "resumed round made exactly one call");
        let msgs = &history[0].messages;
        assert_eq!(msgs.len(), 4, "restored prefix reached the provider");
        assert_eq!(msgs.iter().filter(|m| m.role == Role::User).count(), 1);
        assert_eq!(msgs[3].role, Role::Tool);

        // The goal terminates (max_rounds 1, condition always fails); the
        // terminal checkpoint must have cleared round_messages.
        for _ in 0..200 {
            if store.read().await.load_all().await.is_empty() {
                break;
            }
            let loaded = store.read().await.load_all().await;
            if loaded.iter().all(|s| s.round_messages.is_none()) {
                break;
            }
            drop(loaded);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let final_states = store.read().await.load_all().await;
        assert!(
            final_states.iter().all(|s| s.round_messages.is_none()),
            "terminal checkpoint clears round_messages"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
