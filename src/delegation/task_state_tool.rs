//! The `task_state` tool — how a delegated child agent reads and writes its
//! shared task state.
//!
//! The tool only works inside an active delegation scope (see
//! [`DelegationScope`](crate::delegation::DelegationScope)); outside one it
//! returns a clear error.  It is deliberately **not** in the delegate tool's
//! blocked list — children must be able to share state with their siblings and
//! descendants.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::state::{ArtifactRef, DelegationEvent};
use super::{DelegationError, DelegationTaskStore};
use crate::tools::{Tool, ToolContext, ToolExecutionResult};

/// Tool that lets a delegated agent inspect and mutate its shared task state.
#[derive(Debug, Clone)]
pub struct TaskStateTool {
    store: Arc<DelegationTaskStore>,
    /// Layout root the delegation workspace paths resolve against.
    paths: Arc<crate::dirs::SyscityPaths>,
}

impl TaskStateTool {
    /// Create a new tool backed by the given store.
    pub fn new(store: Arc<DelegationTaskStore>, paths: Arc<crate::dirs::SyscityPaths>) -> Self {
        Self { store, paths }
    }
}

#[async_trait]
impl Tool for TaskStateTool {
    fn name(&self) -> &str {
        "task_state"
    }

    fn description(&self) -> &str {
        r#"Read and write the shared state of your current delegation task.

Use this tool to coordinate with sibling or descendant agents working on the
same task. Each delegated child owns a shared state blob that other agents in
the tree can read.

Your file operations (file_read/file_write) are confined to your delegation
tree's shared workspace. Relative paths resolve into your own task's scratch
dir; write tree-shared files under the absolute `shared_workspace` path (run
`status` to see it) rather than your private `workspace`, so sibling and
descendant agents can read them.

Actions:
- get <key>: read one key from the shared state
- get_from <task_id> <key>: read one key from a sibling or ancestor task's
  shared state (must be in the same delegation tree)
- set <key> <value_json>: write one key to the shared state
- append <text>: append a note to the task's event ledger
- put_artifact <name> <url>: record a reference to an artifact you produced
- handoff <to_agent> <summary>: hand this task to another agent to continue
- status: show the task's status, state, and artifacts

Only available inside an active delegation; errors otherwise."#
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["get", "get_from", "set", "append", "put_artifact", "handoff", "status"],
                    "description": "Action to perform"
                },
                "task_id": {
                    "type": "string",
                    "description": "Target task id in your delegation tree (for get_from)"
                },
                "key": {
                    "type": "string",
                    "description": "State key (for get/set/get_from)"
                },
                "value": {
                    "description": "JSON value to store under key (for set)"
                },
                "text": {
                    "type": "string",
                    "description": "Note to append to the task ledger (for append)"
                },
                "name": {
                    "type": "string",
                    "description": "Artifact name (for put_artifact)"
                },
                "url": {
                    "type": "string",
                    "description": "Artifact URL/path (for put_artifact)"
                },
                "to_agent": {
                    "type": "string",
                    "description": "Agent to hand the task to (for handoff)"
                },
                "summary": {
                    "type": "string",
                    "description": "Handoff summary for the successor (for handoff)"
                }
            },
            "required": ["action"]
        })
    }

    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::tools::sdk::ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["delegation".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args["action"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("action is required".to_string())
        })?;

        let scope = context
            .delegation
            .as_ref()
            .ok_or(DelegationError::NoActiveDelegation)?;
        let task_id = scope.task_id.clone();

        match action {
            "get" => {
                let key = args["key"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("key is required for get".to_string())
                })?;
                let task = self
                    .store
                    .get_task(&task_id)
                    .await?
                    .ok_or_else(|| DelegationError::TaskNotFound(task_id.clone()))?;
                let value = task.state().get(key).cloned();
                Ok(ToolExecutionResult::success(match value.as_ref() {
                    Some(v) => format!("{} = {}", key, v),
                    None => format!("{} is not set", key),
                })
                .with_data(json!({ "key": key, "value": value })))
            }

            "get_from" => {
                let target_id = args["task_id"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "task_id is required for get_from".to_string(),
                    )
                })?;
                let key = args["key"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "key is required for get_from".to_string(),
                    )
                })?;
                let target = self
                    .store
                    .get_task(target_id)
                    .await?
                    .ok_or_else(|| DelegationError::TaskNotFound(target_id.to_string()))?;
                // A delegated agent may only read state from tasks in its own
                // delegation tree — never from an unrelated root.
                if target.root_id != scope.root_id {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "task {} is outside the caller's delegation tree",
                        target_id
                    )));
                }
                let value = target.state().get(key).cloned();
                Ok(ToolExecutionResult::success(match value.as_ref() {
                    Some(v) => format!("{} = {}", key, v),
                    None => format!("{} is not set", key),
                })
                .with_data(json!({ "task_id": target_id, "key": key, "value": value })))
            }

            "set" => {
                let key = args["key"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation("key is required for set".to_string())
                })?;
                let value = args.get("value").ok_or_else(|| {
                    crate::error::SyscityError::Validation("value is required for set".to_string())
                })?;
                let task = self
                    .store
                    .get_task(&task_id)
                    .await?
                    .ok_or_else(|| DelegationError::TaskNotFound(task_id.clone()))?;
                let mut state = task.state();
                state.insert(key.to_string(), value.clone());
                let state_json =
                    serde_json::to_string(&state).map_err(|e| DelegationError::Store(e.into()))?;
                self.store.update_state(&task.id, &state_json).await?;
                self.store
                    .append_event(
                        &task.id,
                        &DelegationEvent::new(&context.user_id, "set_state", key),
                    )
                    .await?;
                Ok(ToolExecutionResult::success(format!("set {} = {}", key, value)))
            }

            "append" => {
                let text = args["text"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "text is required for append".to_string(),
                    )
                })?;
                let task = self
                    .store
                    .get_task(&task_id)
                    .await?
                    .ok_or_else(|| DelegationError::TaskNotFound(task_id.clone()))?;
                self.store
                    .append_event(&task.id, &DelegationEvent::new(&context.user_id, "note", text))
                    .await?;
                Ok(ToolExecutionResult::success("note appended".to_string()))
            }

            "put_artifact" => {
                let name = args["name"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "name is required for put_artifact".to_string(),
                    )
                })?;
                let url = args["url"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "url is required for put_artifact".to_string(),
                    )
                })?;
                let task = self
                    .store
                    .get_task(&task_id)
                    .await?
                    .ok_or_else(|| DelegationError::TaskNotFound(task_id.clone()))?;
                self.store
                    .add_artifact(
                        &task.id,
                        &ArtifactRef {
                            name: name.to_string(),
                            url: url.to_string(),
                            size: 0,
                            producer: context.user_id.clone(),
                        },
                    )
                    .await?;
                self.store
                    .append_event(
                        &task.id,
                        &DelegationEvent::new(&context.user_id, "put_artifact", name),
                    )
                    .await?;
                Ok(ToolExecutionResult::success(format!("recorded artifact {}", name))
                    .with_data(json!({ "name": name, "url": url })))
            }

            "handoff" => {
                let to_agent = args["to_agent"].as_str().ok_or_else(|| {
                    crate::error::SyscityError::Validation(
                        "to_agent is required for handoff".to_string(),
                    )
                })?;
                let summary = args["summary"].as_str().unwrap_or("continue the task");
                let task = self
                    .store
                    .get_task(&task_id)
                    .await?
                    .ok_or_else(|| DelegationError::TaskNotFound(task_id.clone()))?;
                self.store.set_handoff(&task.id, to_agent, summary).await?;
                Ok(ToolExecutionResult::success(format!(
                    "handed task to {} for continuation",
                    to_agent
                )))
            }

            "status" => {
                let task = self
                    .store
                    .get_task(&task_id)
                    .await?
                    .ok_or_else(|| DelegationError::TaskNotFound(task_id.clone()))?;
                let state_keys: Vec<String> = task.state().keys().cloned().collect();
                let workspace = self
                    .paths
                    .delegation_task_dir(&scope.root_id, &scope.task_id);
                let tree_root = self.paths.delegation_workspace_dir(&scope.root_id);
                let shared_root = self.paths.delegation_shared_dir(&scope.root_id);
                Ok(ToolExecutionResult::success(format!(
                    "task {} ({}): status={}, workspace={}, shared_workspace={}, state_keys={:?}, artifacts={}, events={}",
                    task.id,
                    task.title,
                    task.status,
                    workspace.display(),
                    shared_root.display(),
                    state_keys,
                    task.artifacts.len(),
                    task.events.len()
                ))
                .with_data(json!({
                    "task_id": task.id,
                    "status": task.status,
                    "depth": task.depth,
                    "workspace": workspace.display().to_string(),
                    "tree_workspace": tree_root.display().to_string(),
                    "shared_workspace": shared_root.display().to_string(),
                    "state": task.state(),
                    "artifacts": task.artifacts,
                })))
            }

            _ => Err(crate::error::SyscityError::Validation(format!("Unknown action: {}", action))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::delegation::DelegationScope;
    use crate::delegation::NewTask;

    fn scope(task_id: &str) -> DelegationScope {
        DelegationScope::new("root-1", task_id, 2, 3)
    }

    async fn setup() -> TaskStateTool {
        let store = Arc::new(
            DelegationTaskStore::new("sqlite::memory:")
                .await
                .expect("in-memory store"),
        );
        // run-1 and run-2 share a tree rooted at root-1; other-1 belongs to a
        // different tree.
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "worker",
                title: "Test task",
            })
            .await
            .unwrap();
        store
            .create_task(NewTask {
                id: "run-2",
                root_id: "root-1",
                parent_id: Some("run-1"),
                depth: 2,
                agent_id: "sibling",
                title: "Sibling task",
            })
            .await
            .unwrap();
        store
            .create_task(NewTask {
                id: "other-1",
                root_id: "root-other",
                parent_id: None,
                depth: 1,
                agent_id: "stranger",
                title: "Unrelated task",
            })
            .await
            .unwrap();
        TaskStateTool::new(store, crate::dirs::paths())
    }

    fn context_with_scope(task_id: &str) -> ToolContext {
        let ctx = ToolContext::new("child:x", "delegation:run-1");
        ctx.with_delegation(Some(scope(task_id)))
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let tool = setup().await;

        let set = tool
            .execute(
                json!({"action": "set", "key": "url", "value": "https://ex.com/x"}),
                &context_with_scope("run-1"),
            )
            .await
            .unwrap();
        assert!(set.success, "set should succeed: {:?}", set.output);

        let get = tool
            .execute(json!({"action": "get", "key": "url"}), &context_with_scope("run-1"))
            .await
            .unwrap();
        assert!(get.success);
        assert!(get.output.contains("https://ex.com/x"));
    }

    #[tokio::test]
    async fn test_append_and_status() {
        let tool = setup().await;

        tool.execute(json!({"action": "append", "text": "starting"}), &context_with_scope("run-1"))
            .await
            .unwrap();

        let status = tool
            .execute(json!({"action": "status"}), &context_with_scope("run-1"))
            .await
            .unwrap();
        assert!(status.success);
        assert!(status.output.contains("events=1"));
    }

    #[tokio::test]
    async fn test_status_exposes_tree_workspace() {
        let tool = setup().await;
        let status = tool
            .execute(json!({"action": "status"}), &context_with_scope("run-1"))
            .await
            .unwrap();
        assert!(status.success);
        // The agent can discover its file sandbox from the status output.
        assert!(status.output.contains("delegations/root-1/tasks/run-1"));
        let data = status.data.as_ref().unwrap();
        assert!(data["workspace"]
            .as_str()
            .unwrap()
            .ends_with("delegations/root-1/tasks/run-1"));
        assert!(data["tree_workspace"]
            .as_str()
            .unwrap()
            .ends_with("delegations/root-1"));
        // The tree-wide shared dir is where agents write files for siblings.
        assert!(data["shared_workspace"]
            .as_str()
            .unwrap()
            .ends_with("delegations/root-1/shared"));
    }

    #[tokio::test]
    async fn test_put_artifact() {
        let tool = setup().await;

        let result = tool
            .execute(
                json!({"action": "put_artifact", "name": "report.md", "url": "/api/v1/artifacts/report.md"}),
                &context_with_scope("run-1"),
            )
            .await
            .unwrap();
        assert!(result.success);

        let status = tool
            .execute(json!({"action": "status"}), &context_with_scope("run-1"))
            .await
            .unwrap();
        assert!(status.output.contains("artifacts=1"));
    }

    #[tokio::test]
    async fn test_handoff_action() {
        let tool = setup().await;

        let result = tool
            .execute(
                json!({"action": "handoff", "to_agent": "reviewer", "summary": "needs review"}),
                &context_with_scope("run-1"),
            )
            .await
            .unwrap();
        assert!(result.success, "handoff should succeed: {:?}", result.output);

        let status = tool
            .execute(json!({"action": "status"}), &context_with_scope("run-1"))
            .await
            .unwrap();
        assert!(
            status.output.contains("waiting_handoff"),
            "task should be waiting_handoff: {}",
            status.output
        );
    }

    #[tokio::test]
    async fn test_rejects_outside_delegation() {
        let tool = setup().await;
        let plain = ToolContext::new("user", "session-1");
        let result = tool
            .execute(json!({"action": "status"}), &plain)
            .await
            .unwrap_err();
        assert!(
            result.to_string().contains("no active delegation"),
            "unexpected error: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let tool = setup().await;
        let result = tool
            .execute(json!({"action": "explode"}), &context_with_scope("run-1"))
            .await
            .unwrap_err();
        assert!(result.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_get_from_sibling_within_tree() {
        let tool = setup().await;
        // run-2 writes a key; run-1 reads it back via get_from.
        tool.execute(
            json!({"action": "set", "key": "plan", "value": {"steps": 3}}),
            &context_with_scope("run-2"),
        )
        .await
        .unwrap();

        let get_from = tool
            .execute(
                json!({"action": "get_from", "task_id": "run-2", "key": "plan"}),
                &context_with_scope("run-1"),
            )
            .await
            .unwrap();
        assert!(get_from.success, "sibling read should succeed: {:?}", get_from.output);
        assert!(get_from.output.contains("\"steps\":3"));
    }

    #[tokio::test]
    async fn test_get_from_unset_key_reports_not_set() {
        let tool = setup().await;
        let get_from = tool
            .execute(
                json!({"action": "get_from", "task_id": "run-2", "key": "nope"}),
                &context_with_scope("run-1"),
            )
            .await
            .unwrap();
        assert!(get_from.success);
        assert!(get_from.output.contains("nope is not set"));
    }

    #[tokio::test]
    async fn test_get_from_rejects_foreign_tree() {
        let tool = setup().await;
        // other-1 belongs to root-other; run-1 (root-1) may not read it.
        let result = tool
            .execute(
                json!({"action": "get_from", "task_id": "other-1", "key": "x"}),
                &context_with_scope("run-1"),
            )
            .await
            .unwrap_err();
        assert!(
            result
                .to_string()
                .contains("outside the caller's delegation tree"),
            "unexpected error: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_get_from_unknown_task() {
        let tool = setup().await;
        let result = tool
            .execute(
                json!({"action": "get_from", "task_id": "ghost", "key": "x"}),
                &context_with_scope("run-1"),
            )
            .await
            .unwrap_err();
        assert!(result.to_string().contains("ghost"));
    }
}
