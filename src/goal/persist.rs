//! Goal persistence — save/load goal runner state for restart recovery.
//!
//! Goals are persisted as individual JSON files in `~/.syscity/goals/`.
//! On gateway startup, all persisted goals are loaded and resumed. When a goal
//! completes or is aborted, its state file is deleted.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::goal::condition::CheckResult;
use crate::goal::plan::GoalPlan;

/// Directory name for goal state files under `~/.syscity/`.
const GOALS_DIR_NAME: &str = "goals";

/// Get the goals directory path (`~/.syscity/goals`).
pub fn goals_dir() -> PathBuf {
    crate::dirs::syscity_dir().join(GOALS_DIR_NAME)
}

/// Serializable state of a goal runner at a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedGoalState {
    pub goal_id: String,
    pub parent_session_id: String,
    pub plan: GoalPlan,
    pub round: usize,
    pub condition_history: Vec<PersistedRoundResult>,
    /// Set when the goal was blocked by a policy stop (loop detected, round
    /// budget exhausted) instead of finishing. `None` for ordinary
    /// checkpoints; resuming clears it.
    #[serde(default)]
    pub blocked_reason: Option<crate::goal::event::BlockedReason>,
    /// Last validated structured handoff (fresh-context mode only). Carried
    /// across restarts so a resumed goal keeps the same between-round state it
    /// would have had in-process.
    #[serde(default)]
    pub last_handoff: Option<crate::goal::handoff::RoundHandoff>,
    /// Cumulative executor token spend across the goal's LLM calls (cost
    /// axis). `None` for checkpoints written before this field existed or by
    /// providers that do not echo usage.
    #[serde(default)]
    pub token_usage: Option<crate::agent::turns::TurnUsage>,
    /// Message history of an in-flight round (mid-round snapshots only).
    /// `None` for round-end and terminal checkpoints: resuming such a file
    /// starts the round fresh (previous behavior), so any on-disk state
    /// carrying `Some` is genuinely mid-round. Restoring re-sends the prefix
    /// verbatim — OpenAI/DeepSeek passive prefix caching absorbs it;
    /// Anthropic (no request-side cache control) re-prefills at full input
    /// price.
    #[serde(default)]
    pub round_messages: Option<Vec<crate::providers::Message>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Serializable round result for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRoundResult {
    pub round: usize,
    pub results: Vec<CheckResult>,
}

/// File-based goal state store.
///
/// Each goal is stored as `~/.syscity/goals/{goal_id}.json`.
pub struct GoalStore {
    /// `None` means the process-default goals dir, resolved on first use:
    /// constructing a store must not require an installed path root.
    dir: Option<PathBuf>,
}

impl Default for GoalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl GoalStore {
    /// Create a new goal store using the default goals directory.
    pub fn new() -> Self {
        Self { dir: None }
    }

    /// The directory to persist into, resolving the process default on demand.
    fn dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(goals_dir)
    }

    /// Create a goal store with a custom directory (for testing).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }

    /// Ensure the goals directory exists.
    async fn ensure_dir(&self) -> crate::Result<()> {
        if !self.dir().exists() {
            tokio::fs::create_dir_all(self.dir()).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to create goals directory: {:?}", self.dir()),
                    details: e.to_string(),
                }
            })?;
        }
        Ok(())
    }

    /// Path to the state file for a given goal id.
    fn state_path(&self, goal_id: &str) -> PathBuf {
        self.dir().join(format!("{}.json", goal_id))
    }

    /// Save a goal's state to disk.
    ///
    /// The write is atomic: serialise to `path.tmp`, then `rename` over
    /// `path`. A crash mid-write (including a shutdown abort during
    /// `save_checkpoint`) leaves the previous checkpoint intact instead of
    /// truncating it — the same convention as the cron job store.
    pub async fn save(&self, state: &PersistedGoalState) -> crate::Result<()> {
        self.ensure_dir().await?;
        let path = self.state_path(&state.goal_id);
        let json = serde_json::to_string_pretty(state).map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to serialize goal state: {}", e))
        })?;
        let mut tmp_path = path.clone();
        let mut tmp_name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        tmp_name.push(".tmp");
        tmp_path.set_file_name(tmp_name);

        tokio::fs::write(&tmp_path, &json).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to write goal state tmp file: {:?}", tmp_path),
                details: e.to_string(),
            }
        })?;

        tokio::fs::rename(&tmp_path, &path).await.map_err(|e| {
            // Best-effort cleanup of the stale tmp file; ignore the result.
            let _ = std::fs::remove_file(&tmp_path);
            crate::error::SyscityError::Storage {
                context: format!("Failed to finalize goal state: {:?}", path),
                details: e.to_string(),
            }
        })?;
        Ok(())
    }

    /// Load all persisted goal states.
    pub async fn load_all(&self) -> Vec<PersistedGoalState> {
        let mut states = Vec::new();
        let mut entries = match tokio::fs::read_dir(self.dir()).await {
            Ok(e) => e,
            Err(_) => return states,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => match serde_json::from_str::<PersistedGoalState>(&content) {
                    Ok(state) => states.push(state),
                    Err(e) => {
                        tracing::warn!("[goal] Failed to parse persisted state {:?}: {}", path, e);
                    }
                },
                Err(e) => {
                    tracing::warn!("[goal] Failed to read persisted state {:?}: {}", path, e);
                }
            }
        }

        states
    }

    /// Delete a goal's state file.
    pub async fn delete(&self, goal_id: &str) {
        let path = self.state_path(goal_id);
        if path.exists() {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                tracing::warn!("[goal] Failed to delete state file {:?}: {}", path, e);
            }
        }
    }
}

/// Convert runner's internal state to a persisted checkpoint.
#[allow(clippy::too_many_arguments)]
pub fn to_persisted(
    goal_id: &str,
    parent_session_id: &str,
    plan: &GoalPlan,
    round: usize,
    condition_history: &[crate::goal::runner::RoundResult],
    blocked_reason: Option<crate::goal::event::BlockedReason>,
    last_handoff: Option<&crate::goal::handoff::RoundHandoff>,
    token_usage: Option<crate::agent::turns::TurnUsage>,
    round_messages: Option<Vec<crate::providers::Message>>,
) -> PersistedGoalState {
    let now = Utc::now();
    PersistedGoalState {
        goal_id: goal_id.to_string(),
        parent_session_id: parent_session_id.to_string(),
        plan: plan.clone(),
        round,
        condition_history: condition_history
            .iter()
            .map(|r| PersistedRoundResult {
                round: r.round,
                results: r.results.clone(),
            })
            .collect(),
        blocked_reason,
        last_handoff: last_handoff.cloned(),
        token_usage,
        round_messages,
        created_at: now,
        updated_at: now,
    }
}

/// Convert a persisted goal state into parameters for recreating a GoalRunner.
pub fn to_runner_params(
    state: &PersistedGoalState,
) -> (
    String,
    String,
    GoalPlan,
    Vec<crate::goal::runner::RoundResult>,
    Option<crate::agent::turns::TurnUsage>,
) {
    let condition_history: Vec<crate::goal::runner::RoundResult> = state
        .condition_history
        .iter()
        .map(|pr| crate::goal::runner::RoundResult {
            round: pr.round,
            results: pr.results.clone(),
        })
        .collect();

    (
        state.goal_id.clone(),
        state.parent_session_id.clone(),
        state.plan.clone(),
        condition_history,
        state.token_usage,
    )
}

/// Wrapper type for thread-safe shared access to GoalStore.
pub type SharedGoalStore = Arc<RwLock<GoalStore>>;

/// Create a shared goal store (convenience constructor).
pub fn shared_store() -> SharedGoalStore {
    Arc::new(RwLock::new(GoalStore::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::condition::Comparison;
    use crate::goal::runner::RoundResult;

    fn sample_state(goal_id: &str) -> PersistedGoalState {
        PersistedGoalState {
            goal_id: goal_id.to_string(),
            parent_session_id: "session_abc".to_string(),
            plan: crate::goal::GoalPlan::new("write tests").with_condition(
                crate::goal::GoalCondition::ExitCode {
                    command: "cargo test".to_string(),
                    expected: Some(0),
                },
            ),
            round: 2,
            condition_history: vec![PersistedRoundResult {
                round: 1,
                results: vec![crate::goal::CheckResult {
                    condition: crate::goal::GoalCondition::ExitCode {
                        command: "cargo test".to_string(),
                        expected: Some(0),
                    },
                    passed: false,
                    actual: "exit code: 1".to_string(),
                    detail: "tests failed".to_string(),
                }],
            }],
            blocked_reason: None,
            last_handoff: None,
            token_usage: None,
            round_messages: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_to_persisted_round_trip() {
        let condition_history = vec![RoundResult {
            round: 1,
            results: vec![crate::goal::CheckResult {
                condition: crate::goal::GoalCondition::ExitCode {
                    command: "cargo test".to_string(),
                    expected: Some(0),
                },
                passed: true,
                actual: "exit code: 0".to_string(),
                detail: "passed".to_string(),
            }],
        }];
        let plan = crate::goal::GoalPlan::new("test").with_condition(
            crate::goal::GoalCondition::ExitCode {
                command: "true".to_string(),
                expected: Some(0),
            },
        );

        let state = to_persisted(
            "goal_1",
            "session_1",
            &plan,
            3,
            &condition_history,
            None,
            None,
            None,
            None,
        );
        assert_eq!(state.goal_id, "goal_1");
        assert_eq!(state.parent_session_id, "session_1");
        assert_eq!(state.round, 3);
        assert_eq!(state.condition_history.len(), 1);

        let (gid, pid, restored_plan, restored_history, _usage) = to_runner_params(&state);
        assert_eq!(gid, "goal_1");
        assert_eq!(pid, "session_1");
        assert_eq!(restored_plan.description, "test");
        assert_eq!(restored_history.len(), 1);
        assert_eq!(restored_history[0].round, 1);
    }

    #[tokio::test]
    async fn test_goal_store_save_and_load() {
        let dir = std::env::temp_dir().join(format!("goal_test_{}", uuid::Uuid::new_v4()));
        let store = GoalStore::with_dir(dir.clone());

        let state = sample_state("goal_save_test");
        store.save(&state).await.unwrap();

        let loaded = store.load_all().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].goal_id, "goal_save_test");
        assert_eq!(loaded[0].round, 2);

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_goal_store_delete() {
        let dir = std::env::temp_dir().join(format!("goal_test_{}", uuid::Uuid::new_v4()));
        let store = GoalStore::with_dir(dir.clone());

        let state = sample_state("goal_delete_test");
        store.save(&state).await.unwrap();
        assert_eq!(store.load_all().await.len(), 1);

        store.delete("goal_delete_test").await;
        assert_eq!(store.load_all().await.len(), 0);

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_goal_store_load_empty_dir() {
        let dir = std::env::temp_dir().join(format!("goal_test_{}", uuid::Uuid::new_v4()));
        let store = GoalStore::with_dir(dir.clone());

        let loaded = store.load_all().await;
        assert!(loaded.is_empty());

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_blocked_reason_roundtrip() {
        let dir = std::env::temp_dir().join(format!("goal_test_{}", uuid::Uuid::new_v4()));
        let store = GoalStore::with_dir(dir.clone());

        let mut state = sample_state("goal_blocked_test");
        state.blocked_reason = Some(crate::goal::BlockedReason {
            code: crate::goal::BlockedReasonCode::LoopDetected,
            message: "same conditions failed 3 rounds in a row".to_string(),
        });
        store.save(&state).await.unwrap();

        let loaded = store.load_all().await;
        assert_eq!(loaded.len(), 1);
        let reason = loaded[0]
            .blocked_reason
            .as_ref()
            .expect("blocked_reason persisted");
        assert_eq!(reason.code, crate::goal::BlockedReasonCode::LoopDetected);
        assert!(reason.message.contains("3 rounds"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn test_persisted_state_without_blocked_reason_deserializes() {
        // Files written before the blocked_reason field existed must load.
        let json = serde_json::json!({
            "goal_id": "goal_old",
            "parent_session_id": "session_1",
            "plan": {"description": "d", "conditions": [], "max_rounds": 5, "model_override": null},
            "round": 1,
            "condition_history": [],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let state: PersistedGoalState = serde_json::from_value(json).unwrap();
        assert_eq!(state.goal_id, "goal_old");
        assert!(state.blocked_reason.is_none());
    }

    #[test]
    fn test_last_handoff_roundtrip_and_default() {
        use crate::goal::handoff::{HandoffStatus, RoundHandoff};

        // Old files without last_handoff load with None.
        let json = serde_json::json!({
            "goal_id": "goal_old",
            "parent_session_id": "session_1",
            "plan": {"description": "d", "conditions": [], "max_rounds": 5},
            "round": 2,
            "condition_history": [],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let state: PersistedGoalState = serde_json::from_value(json).unwrap();
        assert!(state.last_handoff.is_none());

        // A checkpoint carrying a handoff round-trips through to_persisted.
        let handoff = RoundHandoff {
            status: HandoffStatus::Continue,
            summary: "wrote the skeleton".to_string(),
            next_steps: vec!["fill in chapter 2".to_string()],
            evidence: vec![],
        };
        let plan = GoalPlan::new("d");
        let state = to_persisted("g", "s", &plan, 3, &[], None, Some(&handoff), None, None);
        assert_eq!(state.last_handoff.as_ref().unwrap(), &handoff);
        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedGoalState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.last_handoff.as_ref().unwrap(), &handoff);
    }

    #[test]
    fn test_goals_dir_ends_with_goals() {
        let dir = goals_dir();
        assert!(dir.to_string_lossy().ends_with("goals"));
    }

    #[tokio::test]
    async fn test_goal_store_save_is_atomic_no_tmp_residue() {
        let dir = std::env::temp_dir().join(format!("goal_test_{}", uuid::Uuid::new_v4()));
        let store = GoalStore::with_dir(dir.clone());

        // Save twice so the rename path also covers overwriting an existing
        // checkpoint; a crash mid-write must never truncate the live file.
        store.save(&sample_state("goal_atomic_test")).await.unwrap();
        let mut updated = sample_state("goal_atomic_test");
        updated.round = 7;
        store.save(&updated).await.unwrap();

        let loaded = store.load_all().await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].round, 7);

        // No .tmp residue left behind, and it must not be picked up as a
        // goal checkpoint.
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "stale tmp file left: {name}");
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn test_token_usage_roundtrip_and_default() {
        use crate::agent::turns::TurnUsage;

        // Old files without token_usage load with None.
        let json = serde_json::json!({
            "goal_id": "goal_old",
            "parent_session_id": "session_1",
            "plan": {"description": "d", "conditions": [], "max_rounds": 5},
            "round": 1,
            "condition_history": [],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let state: PersistedGoalState = serde_json::from_value(json).unwrap();
        assert!(state.token_usage.is_none());

        // A checkpoint carrying usage round-trips through serialization.
        let usage = TurnUsage {
            prompt_tokens: 1_000,
            completion_tokens: 250,
            total_tokens: 1_250,
            ..Default::default()
        };
        let plan = GoalPlan::new("d");
        let state = to_persisted("g", "s", &plan, 2, &[], None, None, Some(usage), None);
        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedGoalState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.token_usage, Some(usage));
    }

    #[test]
    fn test_round_messages_roundtrip_and_default() {
        use crate::providers::{FunctionCall, Message, ToolCall};

        // Old files without round_messages load with None.
        let json = serde_json::json!({
            "goal_id": "goal_old",
            "parent_session_id": "session_1",
            "plan": {"description": "d", "conditions": [], "max_rounds": 5},
            "round": 1,
            "condition_history": [],
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        });
        let state: PersistedGoalState = serde_json::from_value(json).unwrap();
        assert!(state.round_messages.is_none());

        // A mid-round snapshot carrying all four message shapes round-trips
        // through serialization, including tool_call ids and roles.
        let messages = vec![
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
        ];
        let plan = GoalPlan::new("d");
        let state = to_persisted("g", "s", &plan, 1, &[], None, None, None, Some(messages.clone()));
        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedGoalState = serde_json::from_str(&json).unwrap();
        let restored_msgs = restored.round_messages.expect("round_messages persisted");
        assert_eq!(restored_msgs.len(), 4);
        assert_eq!(restored_msgs[3].role, crate::providers::Role::Tool);
        assert_eq!(restored_msgs[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(restored_msgs[2].tool_calls.as_ref().unwrap()[0].id, "call_1");
    }
}
