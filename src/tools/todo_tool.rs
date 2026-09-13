//! Todo Tool - Task management for the agent
//!
//! The tool uses whole-snapshot semantics: every call carries the COMPLETE
//! new task list, and writing it atomically replaces the stored state
//! (last write wins — there is no partial merge). This eliminates
//! partial-update corner states and stale checklists.
//!
//! State is held in [`TodoState`] behind an `Arc`, shared between the tool
//! and the [`crate::tools::ToolRegistry`]. The engine clears a conversation's
//! snapshot at the start of each new user turn so the UI never shows a stale
//! checklist.
//!
//! Snapshots persist to disk in ~/.syscity/todos/{conversation_id}.json

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::agent::todo::{TaskStatus, TodoStore};
use crate::tools::sdk::ToolCapabilities;

/// Shared per-conversation todo state (in-memory cache + disk persistence).
///
/// Both the [`TodoTool`] and the agent engine hold an `Arc<TodoState>`,
/// which lets the engine clear the active plan for a conversation when a
/// new user turn begins without reaching into the boxed tool instance.
#[derive(Debug)]
pub struct TodoState {
    /// In-memory storage of todo lists per conversation
    stores: RwLock<HashMap<String, TodoStore>>,
    /// Base directory for todo files. `None` means the process-default todos
    /// dir, resolved on first use: constructing a tool must not require an
    /// installed path root.
    base_dir: Option<PathBuf>,
}

impl Default for TodoState {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoState {
    /// Create state backed by the default todos directory.
    ///
    /// The directory is resolved on first use, so building a tool never needs a
    /// process path root to be installed.
    pub fn new() -> Self {
        Self {
            stores: RwLock::new(HashMap::new()),
            base_dir: None,
        }
    }

    /// The directory to persist into, resolving the process default on demand.
    fn base_dir(&self) -> PathBuf {
        self.base_dir.clone().unwrap_or_else(crate::dirs::todos_dir)
    }

    /// Create with custom directory (for testing)
    pub fn with_dir(base_dir: PathBuf) -> Self {
        Self {
            stores: RwLock::new(HashMap::new()),
            base_dir: Some(base_dir),
        }
    }

    /// Get the file path for a conversation's todo file
    fn todo_file_path(&self, conversation_id: &str) -> PathBuf {
        // Sanitize conversation ID to be safe for filenames
        let safe_id =
            conversation_id.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
        self.base_dir().join(format!("{}.json", safe_id))
    }

    /// Load a todo store from disk
    async fn load_from_disk(&self, conversation_id: &str) -> Option<TodoStore> {
        let path = self.todo_file_path(conversation_id);

        if !path.exists() {
            return None;
        }

        debug!("Loading todo store from {:?}", path);

        match tokio::fs::read_to_string(&path).await {
            Ok(content) => match TodoStore::from_json(&content) {
                Ok(store) => {
                    debug!("Loaded {} tasks for conversation {}", store.count(), conversation_id);
                    Some(store)
                }
                Err(e) => {
                    error!("Failed to parse todo file {:?}: {}", path, e);
                    None
                }
            },
            Err(e) => {
                error!("Failed to read todo file {:?}: {}", path, e);
                None
            }
        }
    }

    /// Get (or create) the in-store snapshot for a conversation.
    ///
    /// Prefers the in-memory cache, then falls back to the persisted file,
    /// then starts from an empty store. The monotonic task-ID counter inside
    /// the loaded store keeps IDs unique across whole-snapshot replaces.
    pub async fn get_store(&self, conversation_id: &str) -> TodoStore {
        // First check in-memory cache
        {
            let stores = self.stores.read().await;
            if let Some(store) = stores.get(conversation_id) {
                return store.clone();
            }
        }

        // Try to load from disk
        if let Some(store) = self.load_from_disk(conversation_id).await {
            let mut stores = self.stores.write().await;
            stores.insert(conversation_id.to_string(), store.clone());
            return store;
        }

        // Create new store
        let store = TodoStore::new();
        let mut stores = self.stores.write().await;
        stores.insert(conversation_id.to_string(), store.clone());
        store
    }

    /// Persist the snapshot for a conversation (memory + disk).
    ///
    /// The file write goes through a temp file + rename so readers never see
    /// a torn half-written snapshot.
    pub async fn save_store(&self, conversation_id: &str, store: TodoStore) -> crate::Result<()> {
        let path = self.todo_file_path(conversation_id);

        debug!("Saving todo store to {:?}", path);

        // Fresh installs have no todos directory yet; create it lazily.
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                error!("Failed to create todo directory {:?}: {}", parent, e);
                return Err(crate::error::SyscityError::Storage {
                    context: format!("Failed to create todo directory: {:?}", parent),
                    details: e.to_string(),
                });
            }
        }

        let json = store.to_json()?;
        // Unique tmp name per save: two parallel writes to the same
        // conversation file must not consume each other's temp file.
        let tmp_path = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
        if let Err(e) = tokio::fs::write(&tmp_path, &json).await {
            error!("Failed to write todo temp file {:?}: {}", tmp_path, e);
            return Err(crate::error::SyscityError::Storage {
                context: format!("Failed to write todo temp file: {:?}", tmp_path),
                details: e.to_string(),
            });
        }
        if let Err(e) = tokio::fs::rename(&tmp_path, &path).await {
            error!("Failed to rename todo file {:?} -> {:?}: {}", tmp_path, path, e);
            return Err(crate::error::SyscityError::Storage {
                context: format!("Failed to persist todo file: {:?}", path),
                details: e.to_string(),
            });
        }

        debug!("Saved {} tasks for conversation {}", store.count(), conversation_id);

        // Update in-memory cache only after the disk write succeeded.
        let mut stores = self.stores.write().await;
        stores.insert(conversation_id.to_string(), store);
        Ok(())
    }

    /// Clear the active plan for a conversation (memory + disk).
    ///
    /// Called by the engine at the start of every new user turn: "the
    /// currently effective plan" is the most recent turn's todo, so a new
    /// turn automatically clears it and the UI never shows a stale
    /// checklist. Best-effort: failures are logged, never fatal.
    pub async fn clear_conversation(&self, conversation_id: &str) {
        {
            let mut stores = self.stores.write().await;
            stores.remove(conversation_id);
        }

        let path = self.todo_file_path(conversation_id);
        if !path.exists() {
            return;
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                debug!("Cleared todo snapshot for conversation {}", conversation_id);
            }
            Err(e) => {
                warn!(
                    "Failed to remove todo file {:?} for conversation {}: {}",
                    path, conversation_id, e
                );
            }
        }
    }

    /// Clean up old completed tasks across all conversations
    /// Returns number of tasks cleaned up
    pub async fn cleanup_old_completed(&self, max_age_days: i64) -> usize {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(max_age_days);
        let mut total_cleaned = 0;

        // Get list of all todo files
        let mut entries = match tokio::fs::read_dir(self.base_dir()).await {
            Ok(entries) => entries,
            Err(e) => {
                error!("Failed to read todos directory: {}", e);
                return 0;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            let conversation_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            if let Some(mut store) = self.load_from_disk(&conversation_id).await {
                let before_count = store.count();

                // Remove old completed tasks
                let old_completed: Vec<String> = store
                    .list()
                    .into_iter()
                    .filter(|t| {
                        t.status == TaskStatus::Completed
                            && t.completed_at.map(|t| t < cutoff).unwrap_or(false)
                    })
                    .map(|t| t.id.clone())
                    .collect();

                for task_id in old_completed {
                    store.remove(&task_id);
                    total_cleaned += 1;
                }

                // If store is empty, delete the file
                if store.count() == 0 {
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        warn!("Failed to remove empty todo file {:?}: {}", path, e);
                    } else {
                        debug!("Removed empty todo file {:?}", path);
                    }
                } else if store.count() != before_count {
                    // Save if we removed some tasks
                    if let Err(e) = self.save_store(&conversation_id, store.clone()).await {
                        warn!("Failed to save cleaned todo store: {}", e);
                        continue;
                    }

                    // Update cache if present
                    let mut stores = self.stores.write().await;
                    if stores.contains_key(&conversation_id) {
                        stores.insert(conversation_id, store);
                    }
                }
            }
        }

        if total_cleaned > 0 {
            info!("Cleaned up {} old completed tasks", total_cleaned);
        }

        total_cleaned
    }

    /// List all conversations with todos
    pub async fn list_conversations(&self) -> Vec<String> {
        let mut conversations = Vec::new();

        let mut entries = match tokio::fs::read_dir(self.base_dir()).await {
            Ok(entries) => entries,
            Err(_) => return conversations,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    conversations.push(stem.to_string());
                }
            }
        }

        conversations
    }
}

/// One task entry in a whole-snapshot write.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskInput {
    /// Task content/description
    pub content: String,
    /// Task status; defaults to pending
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// Task priority 1-5 (1=highest); defaults to medium (3)
    #[serde(default)]
    pub priority: Option<u8>,
}

/// Tool for managing tasks/todos via whole-snapshot writes
#[derive(Debug)]
pub struct TodoTool {
    /// Shared todo state, also held by the registry for turn-start resets
    state: Arc<TodoState>,
}

impl TodoTool {
    /// Create a new todo tool with its own private state
    pub fn new() -> Self {
        Self {
            state: Arc::new(TodoState::new()),
        }
    }

    /// Create a todo tool sharing `state`.
    ///
    /// Used by the gateway so the agent engine can clear the active plan
    /// through the same handle the tool writes to.
    pub fn with_state(state: Arc<TodoState>) -> Self {
        Self { state }
    }

    /// Create with custom directory (for testing)
    #[cfg(test)]
    pub fn with_dir(base_dir: PathBuf) -> Self {
        Self {
            state: Arc::new(TodoState::with_dir(base_dir)),
        }
    }

    /// The shared state backing this tool.
    pub fn state(&self) -> Arc<TodoState> {
        self.state.clone()
    }
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        r#"Write the agent's task list (todo checklist).

The input IS the complete new task list: EVERY call atomically replaces the
entire stored list (last write wins - there is no merge or incremental update).
To add a task, resend the full list including it. To drop a task, omit it.
To clear the list, send an empty array.

Rules:
- Always send the complete list, including unchanged tasks with their current
  status.
- Keep exactly ONE task in_progress at a time.
- Mark tasks completed as soon as they finish, then resend the updated list.

Use this tool for complex tasks with 3+ steps to track progress and ensure
completion. Snapshots persist across daemon restarts and are automatically
cleared when a new user turn begins."#
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The COMPLETE new task list; fully replaces the previous list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Task content/description"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": "Task status (defaults to pending)"
                            },
                            "priority": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 5,
                                "description": "Task priority 1-5 (1=highest, 5=lowest; defaults to 3)"
                            }
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Low,
            categories: vec!["task".to_string(), "management".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let todos_value = args.get("todos").ok_or_else(|| {
            crate::error::SyscityError::Validation(
                "todos is required: pass the complete task list (possibly empty)".to_string(),
            )
        })?;
        let items: Vec<TaskInput> = serde_json::from_value(todos_value.clone()).map_err(|e| {
            crate::error::SyscityError::Validation(format!(
                "Invalid todos array (each item needs content, plus optional status/priority): {}",
                e
            ))
        })?;

        let conversation_id = &context.conversation_id;

        // Start from the existing store so its monotonic ID counter survives
        // across snapshots, then replace its contents wholesale.
        let mut store = self.state.get_store(conversation_id).await;
        let specs = items.into_iter().map(|item| {
            (
                item.content,
                item.status.unwrap_or(TaskStatus::Pending),
                item.priority.unwrap_or(3),
            )
        });
        let tasks = store.replace_tasks(specs);

        let total = tasks.len();
        let active = tasks.iter().filter(|t| t.is_active()).count();
        let formatted = store.format_for_prompt();
        self.state.save_store(conversation_id, store).await?;

        let output = if total == 0 {
            "Task list cleared.".to_string()
        } else {
            format!("Task list replaced ({} tasks, {} active).\n{}", total, active, formatted)
        };

        Ok(ToolExecutionResult::success(output).with_data(json!({
            "tasks": tasks
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "content": t.content,
                        "status": t.status.to_string(),
                        "priority": t.priority,
                    })
                })
                .collect::<Vec<_>>(),
            "total": total,
            "active": active
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_todo_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("syscity_todo_test_{}_{}", tag, uuid::Uuid::new_v4()))
    }

    async fn write_snapshot(tool: &TodoTool, ctx: &ToolContext, items: serde_json::Value) {
        tool.execute(json!({ "todos": items }), ctx).await.unwrap();
    }

    #[tokio::test]
    async fn test_todo_snapshot_replace_removes_stale_tasks() {
        let temp_dir = temp_todo_dir("replace");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let tool = TodoTool::with_dir(temp_dir.clone());
        let ctx = ToolContext::new("user", "conv_replace");

        write_snapshot(
            &tool,
            &ctx,
            json!([
                {"content": "Old task A"},
                {"content": "Old task B", "status": "in_progress"}
            ]),
        )
        .await;

        // Second write carries the complete NEW list: removed tasks are gone.
        let result = tool
            .execute(json!({"todos": [{"content": "New task C", "status": "in_progress"}]}), &ctx)
            .await
            .unwrap();

        assert!(result.output.contains("New task C"));
        assert!(!result.output.contains("Old task A"), "removed task must disappear");
        let data = result.data.unwrap();
        assert_eq!(data["total"], 1);
        assert_eq!(data["tasks"][0]["status"], "in_progress");

        // The persisted file reflects exactly the latest snapshot.
        let raw = tokio::fs::read_to_string(temp_dir.join("conv_replace.json"))
            .await
            .unwrap();
        assert!(raw.contains("New task C"));
        assert!(!raw.contains("Old task A"));

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_todo_snapshot_empty_clears_all() {
        let temp_dir = temp_todo_dir("empty");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let tool = TodoTool::with_dir(temp_dir.clone());
        let ctx = ToolContext::new("user", "conv_empty");

        write_snapshot(&tool, &ctx, json!([{"content": "Only task"}])).await;

        let result = tool.execute(json!({"todos": []}), &ctx).await.unwrap();
        assert!(result.output.contains("cleared"));

        let data = result.data.unwrap();
        assert_eq!(data["total"], 0);

        // A follow-up write starts fresh.
        write_snapshot(&tool, &ctx, json!([{"content": "Fresh task"}])).await;
        let raw = tokio::fs::read_to_string(temp_dir.join("conv_empty.json"))
            .await
            .unwrap();
        assert!(raw.contains("Fresh task"));
        assert!(!raw.contains("Only task"));

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_todo_missing_todos_arg_is_validation_error() {
        let tool = TodoTool::with_dir(temp_todo_dir("missing"));
        let ctx = ToolContext::new("user", "conv_missing");

        let err = tool.execute(json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("todos is required"));

        let err = tool
            .execute(json!({"todos": [{"nope": true}]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid todos array"));
    }

    #[tokio::test]
    async fn test_todo_persistence_across_instances() {
        let temp_dir = temp_todo_dir("persist");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        // Create first tool instance and write a snapshot.
        {
            let tool = TodoTool::with_dir(temp_dir.clone());
            let ctx = ToolContext::new("user", "persistent_conv");
            write_snapshot(&tool, &ctx, json!([{"content": "Persistent task"}])).await;
        }

        // Create second tool instance (simulating daemon restart).
        {
            let tool = TodoTool::with_dir(temp_dir.clone());
            let ctx = ToolContext::new("user", "persistent_conv");
            // A whole-snapshot write that KEEPS the existing task must not
            // collide IDs with pre-restart tasks.
            let result = tool
                .execute(
                    json!({"todos": [
                        {"content": "Persistent task", "status": "completed"},
                        {"content": "Second generation task"}
                    ]}),
                    &ctx,
                )
                .await
                .unwrap();

            let data = result.data.unwrap();
            assert_eq!(data["total"], 2);
            let ids: Vec<&str> = data["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t["id"].as_str())
                .collect();
            assert_eq!(ids.len(), 2);
            assert_ne!(ids[0], ids[1], "regenerated IDs must stay unique");
        }

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_todo_state_clear_conversation() {
        let temp_dir = temp_todo_dir("clear");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let state = Arc::new(TodoState::with_dir(temp_dir.clone()));
        let tool = TodoTool::with_state(state.clone());
        let ctx = ToolContext::new("user", "conv_clear");

        write_snapshot(&tool, &ctx, json!([{"content": "Stale checklist"}])).await;
        assert!(temp_dir.join("conv_clear.json").exists());

        state.clear_conversation("conv_clear").await;

        // Memory entry gone...
        let stores = state.stores.read().await;
        assert!(!stores.contains_key("conv_clear"));
        drop(stores);
        // ...and the persisted snapshot deleted.
        assert!(!temp_dir.join("conv_clear.json").exists());

        // Clearing again (nothing left) is a no-op, not an error.
        state.clear_conversation("conv_clear").await;

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_shared_state_visible_between_tool_instances() {
        let temp_dir = temp_todo_dir("shared");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let state = Arc::new(TodoState::with_dir(temp_dir.clone()));

        let writer = TodoTool::with_state(state.clone());
        let reader = TodoTool::with_state(state.clone());
        let ctx = ToolContext::new("user", "conv_shared");

        write_snapshot(&writer, &ctx, json!([{"content": "Shared task"}])).await;

        // The engine-facing handle sees the same snapshot without touching disk.
        let store = state.get_store("conv_shared").await;
        assert_eq!(store.count(), 1);
        assert_eq!(store.list()[0].content, "Shared task");

        // And so does another tool instance over the same handle.
        let store = reader.state.get_store("conv_shared").await;
        assert_eq!(store.count(), 1);

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_todo_cleanup() {
        let temp_dir = temp_todo_dir("cleanup");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        let tool = TodoTool::with_dir(temp_dir.clone());
        let state = tool.state();

        {
            let ctx = ToolContext::new("user", "cleanup_conv");
            write_snapshot(&tool, &ctx, json!([{"content": "Old task"}])).await;
        }

        // Manually modify the file to make the task old
        // Create JSON with an old completed_at date directly
        let todo_file = temp_dir.join("cleanup_conv.json");
        let old_date = (chrono::Utc::now() - chrono::Duration::days(40)).to_rfc3339();
        let modified = format!(
            r#"{{"tasks":{{"task_1":{{"id":"task_1","content":"Old task","status":"completed","created_at":"{}","updated_at":"{}","completed_at":"{}","parent_id":null,"subtasks":[],"priority":3,"metadata":{{}}}}}},"order":["task_1"],"next_id":2}}"#,
            old_date, old_date, old_date
        );
        tokio::fs::write(&todo_file, modified).await.unwrap();

        // Run cleanup (30 days)
        let cleaned = state.cleanup_old_completed(30).await;
        assert_eq!(cleaned, 1);

        // Verify file was removed (since it was empty after cleanup)
        assert!(!todo_file.exists());

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
