//! Persistent shared task state for delegation trees.
//!
//! Each delegated child task gets a row in the `delegation_tasks` table.  The
//! row is the canonical shared state: a JSON key/value blob, an append-only
//! events ledger, and artifact references produced by the child.  Sibling and
//! descendant agents read and update this state through the `task_state` tool,
//! which gives syscity the shared-work tracking LoopX gets from its canonical
//! state body + event ledger.
//!
//! Storage mirrors [`crate::planner::state::TaskStateStore`]: a small sqlx
//! SQLite pool with the parent directory created on demand.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePoolOptions, Pool, Row, Sqlite};
use tracing::{info, instrument};

/// One appended event in a delegation task's ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationEvent {
    /// RFC3339 timestamp (seconds precision is fine for an audit trail).
    pub ts: String,
    /// Agent id that produced the event.
    pub agent: String,
    /// Action performed (e.g. "set_state", "put_artifact", "handoff").
    pub action: String,
    /// Free-form detail (short).
    pub detail: String,
}

impl DelegationEvent {
    /// Create a new event with the current time.
    pub fn new(agent: impl AsRef<str>, action: impl AsRef<str>, detail: impl AsRef<str>) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339(),
            agent: agent.as_ref().to_string(),
            action: action.as_ref().to_string(),
            detail: detail.as_ref().to_string(),
        }
    }
}

/// A reference to an artifact produced by a delegated task.  The bytes live in
/// the shared artifacts directory (`~/.syscity/artifacts/`); the row only
/// records the reference and its producer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRef {
    /// Short human name (e.g. "report.md").
    pub name: String,
    /// Public URL/path (e.g. "/api/v1/artifacts/<file>").
    pub url: String,
    /// Size in bytes, when known.
    pub size: u64,
    /// Agent id that produced the artifact.
    pub producer: String,
}

/// Full read model of one delegation task row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationTask {
    /// Task id (equal to the registry run id).
    pub id: String,
    /// Root of the delegation tree.
    pub root_id: String,
    /// Parent task id (`None` for the root of a tree).
    pub parent_id: Option<String>,
    /// Nesting depth (top-level delegation = 1).
    pub depth: u32,
    /// Agent id currently responsible for the task.
    pub agent_id: String,
    /// Short human title.
    pub title: String,
    /// Status: `pending | running | completed | failed | waiting_handoff`.
    pub status: String,
    /// Shared JSON key/value state.
    pub state_json: String,
    /// Artifact references produced by this task.
    pub artifacts: Vec<ArtifactRef>,
    /// Append-only events ledger.
    pub events: Vec<DelegationEvent>,
    /// RFC3339 creation time.
    pub created_at: String,
    /// RFC3339 last update time.
    pub updated_at: String,
    /// RFC3339 completion time (`None` while active).
    pub completed_at: Option<String>,
}

impl DelegationTask {
    /// Parse the shared state blob as a JSON map, tolerating an unparseable
    /// body (returns an empty map).
    pub fn state(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(&self.state_json).unwrap_or_default()
    }

    /// Whether the task is waiting for a handoff successor.
    pub fn is_waiting_handoff(&self) -> bool {
        self.status == "waiting_handoff"
    }
}

/// Parameters for creating a new delegation task row.
#[derive(Debug, Clone)]
pub struct NewTask<'a> {
    /// Task id (registry run id).
    pub id: &'a str,
    /// Root of the delegation tree.
    pub root_id: &'a str,
    /// Parent task id (`None` for a tree root).
    pub parent_id: Option<&'a str>,
    /// Nesting depth.
    pub depth: u32,
    /// Agent id responsible for the task.
    pub agent_id: &'a str,
    /// Short human title.
    pub title: &'a str,
}

/// SQLite-backed shared task state store for delegation trees.
#[derive(Debug, Clone)]
pub struct DelegationTaskStore {
    pool: Pool<Sqlite>,
}

impl DelegationTaskStore {
    /// Create a new store at the given database URL.
    ///
    /// Example: `sqlite://~/.syscity/data/delegations.db`
    pub async fn new(database_url: &str) -> crate::Result<Self> {
        info!("Initializing delegation task store");

        if database_url.starts_with("sqlite://") && !database_url.contains(":memory:") {
            let path_str = database_url
                .strip_prefix("sqlite://")
                .unwrap_or(database_url);
            let path = std::path::Path::new(path_str);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: format!(
                            "Failed to create delegation task store directory: {:?}",
                            parent
                        ),
                        details: e.to_string(),
                    }
                })?;
            }
            // sqlx 0.8 defaults `create_if_missing` to false; explicitly create
            // the file so a fresh install can open the database (mirrors
            // `gateway/init/storage.rs`).
            if !path.exists() {
                tokio::fs::File::create(path).await.map_err(|e| {
                    crate::error::SyscityError::Storage {
                        context: format!("Failed to create delegation task store file: {:?}", path),
                        details: e.to_string(),
                    }
                })?;
            }
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .acquire_timeout(Duration::from_secs(30))
            .connect(database_url)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to connect to delegation task database".to_string(),
                details: e.to_string(),
            })?;

        let store = Self { pool };
        store.init_schema().await?;
        Ok(store)
    }

    async fn init_schema(&self) -> crate::Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS delegation_tasks (
                id           TEXT PRIMARY KEY,
                root_id      TEXT NOT NULL,
                parent_id    TEXT,
                depth        INTEGER NOT NULL DEFAULT 0,
                agent_id     TEXT,
                title        TEXT,
                status       TEXT NOT NULL DEFAULT 'pending',
                state_json   TEXT NOT NULL DEFAULT '{}',
                artifacts_json TEXT NOT NULL DEFAULT '[]',
                events_json  TEXT NOT NULL DEFAULT '[]',
                created_at   TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create delegation_tasks table".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_delegation_tasks_root ON delegation_tasks(root_id)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to create delegation_tasks root index".to_string(),
            details: e.to_string(),
        })?;

        Ok(())
    }

    /// Create a new task row.  Returns the task id on success.
    #[instrument(skip(self))]
    pub async fn create_task(&self, params: NewTask<'_>) -> crate::Result<String> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO delegation_tasks (
                id, root_id, parent_id, depth, agent_id, title,
                status, created_at, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?7)
            "#,
        )
        .bind(params.id)
        .bind(params.root_id)
        .bind(params.parent_id)
        .bind(params.depth as i64)
        .bind(params.agent_id)
        .bind(params.title)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to create delegation task '{}'", params.id),
            details: e.to_string(),
        })?;

        Ok(params.id.to_string())
    }

    /// Mark tasks left in `running`/`waiting_handoff` by a previous process
    /// as failed. Called once at startup: those rows belong to executions
    /// that died with the last process and will never settle on their own.
    /// Returns the number of rows swept.
    pub async fn fail_orphaned_runs(&self) -> crate::Result<u64> {
        let now = chrono::Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE delegation_tasks SET status = 'failed', updated_at = ?1 \
             WHERE status IN ('running', 'waiting_handoff')",
        )
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: "Failed to sweep orphaned delegation tasks".to_string(),
            details: e.to_string(),
        })?;
        Ok(result.rows_affected())
    }

    /// Load one task row.
    pub async fn get_task(&self, id: &str) -> crate::Result<Option<DelegationTask>> {
        let row = sqlx::query("SELECT * FROM delegation_tasks WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to load delegation task '{}'", id),
                details: e.to_string(),
            })?;

        match row {
            Some(r) => Ok(Some(read_task_row(&r)?)),
            None => Ok(None),
        }
    }

    /// Replace the shared state JSON blob for a task.
    pub async fn update_state(&self, id: &str, state_json: &str) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("UPDATE delegation_tasks SET state_json = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(state_json)
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to update delegation task '{}' state", id),
                details: e.to_string(),
            })?;
        Ok(())
    }

    /// Append one event to the task's ledger.  Read-modify-write on the JSON
    /// blob; safe because each task is owned by a single running child.
    pub async fn append_event(&self, id: &str, event: &DelegationEvent) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut events = self.load_events(id).await?;
        events.push(event.clone());
        let json =
            serde_json::to_string(&events).map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to serialize events for task '{}'", id),
                details: e.to_string(),
            })?;
        sqlx::query("UPDATE delegation_tasks SET events_json = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(json)
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to append event to task '{}'", id),
                details: e.to_string(),
            })?;
        Ok(())
    }

    /// Append an artifact reference to the task's artifact list.
    pub async fn add_artifact(&self, id: &str, artifact: &ArtifactRef) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut artifacts = self.load_artifacts(id).await?;
        artifacts.push(artifact.clone());
        let json =
            serde_json::to_string(&artifacts).map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to serialize artifacts for task '{}'", id),
                details: e.to_string(),
            })?;
        sqlx::query(
            "UPDATE delegation_tasks SET artifacts_json = ?1, updated_at = ?2 WHERE id = ?3",
        )
        .bind(json)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to add artifact to task '{}'", id),
            details: e.to_string(),
        })?;
        Ok(())
    }

    /// Set the task status and, for terminal states, its completion time.
    pub async fn set_status(&self, id: &str, status: &str) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let is_terminal = matches!(status, "completed" | "failed");
        sqlx::query(
            r#"
            UPDATE delegation_tasks
            SET status = ?1, updated_at = ?2,
                completed_at = CASE WHEN ?3 = 1 THEN ?2 ELSE completed_at END
            WHERE id = ?4
            "#,
        )
        .bind(status)
        .bind(&now)
        .bind(if is_terminal { 1i64 } else { 0i64 })
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to set delegation task '{}' status", id),
            details: e.to_string(),
        })?;
        Ok(())
    }

    /// Record a handoff request: the current agent names a successor and hands
    /// the task over.  Status becomes `waiting_handoff`.
    pub async fn set_handoff(&self, id: &str, to_agent: &str, summary: &str) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let event =
            DelegationEvent::new("agent", "handoff", format!("to {}: {}", to_agent, summary));
        let mut events = self.load_events(id).await?;
        events.push(event);
        let events_json =
            serde_json::to_string(&events).map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to serialize events for task '{}'", id),
                details: e.to_string(),
            })?;

        sqlx::query(
            r#"
            UPDATE delegation_tasks
            SET status = 'waiting_handoff', agent_id = ?1, events_json = ?2, updated_at = ?3
            WHERE id = ?4
            "#,
        )
        .bind(to_agent)
        .bind(events_json)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to set handoff for delegation task '{}'", id),
            details: e.to_string(),
        })?;
        Ok(())
    }

    /// Find the oldest `waiting_handoff` task under a root tree.
    pub async fn pending_handoff_for_root(
        &self,
        root_id: &str,
    ) -> crate::Result<Option<DelegationTask>> {
        let row = sqlx::query(
            r#"
            SELECT * FROM delegation_tasks
            WHERE root_id = ?1 AND status = 'waiting_handoff'
            ORDER BY created_at ASC
            LIMIT 1
            "#,
        )
        .bind(root_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to query handoffs for root '{}'", root_id),
            details: e.to_string(),
        })?;

        match row {
            Some(r) => Ok(Some(read_task_row(&r)?)),
            None => Ok(None),
        }
    }

    /// All tasks under a root tree, oldest first.
    pub async fn tasks_for_root(&self, root_id: &str) -> crate::Result<Vec<DelegationTask>> {
        let rows = sqlx::query(
            "SELECT * FROM delegation_tasks WHERE root_id = ?1 ORDER BY created_at ASC",
        )
        .bind(root_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to load tasks for root '{}'", root_id),
            details: e.to_string(),
        })?;

        rows.iter().map(read_task_row).collect()
    }

    async fn load_events(&self, id: &str) -> crate::Result<Vec<DelegationEvent>> {
        let row = sqlx::query("SELECT events_json FROM delegation_tasks WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to load events for task '{}'", id),
                details: e.to_string(),
            })?;
        let json: String = match row {
            Some(r) => r
                .try_get("events_json")
                .unwrap_or_else(|_| "[]".to_string()),
            None => return Ok(Vec::new()),
        };
        serde_json::from_str(&json).map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to parse events for task '{}'", id),
            details: e.to_string(),
        })
    }

    async fn load_artifacts(&self, id: &str) -> crate::Result<Vec<ArtifactRef>> {
        let row = sqlx::query("SELECT artifacts_json FROM delegation_tasks WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to load artifacts for task '{}'", id),
                details: e.to_string(),
            })?;
        let json: String = match row {
            Some(r) => r
                .try_get("artifacts_json")
                .unwrap_or_else(|_| "[]".to_string()),
            None => return Ok(Vec::new()),
        };
        serde_json::from_str(&json).map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to parse artifacts for task '{}'", id),
            details: e.to_string(),
        })
    }
}

/// Read one `delegation_tasks` row into a [`DelegationTask`].
fn read_task_row(row: &sqlx::sqlite::SqliteRow) -> crate::Result<DelegationTask> {
    let id: String = row.try_get("id").map_err(storage_err("id"))?;
    let root_id: String = row.try_get("root_id").map_err(storage_err("root_id"))?;
    let parent_id: Option<String> = row.try_get("parent_id").ok().flatten();
    let depth: i64 = row.try_get("depth").map_err(storage_err("depth"))?;
    let agent_id: Option<String> = row.try_get("agent_id").ok().flatten();
    let title: Option<String> = row.try_get("title").ok().flatten();
    let status: String = row.try_get("status").map_err(storage_err("status"))?;
    let state_json: String = row
        .try_get("state_json")
        .map_err(storage_err("state_json"))?;
    let artifacts_json: String = row
        .try_get("artifacts_json")
        .map_err(storage_err("artifacts_json"))?;
    let events_json: String = row
        .try_get("events_json")
        .map_err(storage_err("events_json"))?;
    let created_at: String = row
        .try_get("created_at")
        .map_err(storage_err("created_at"))?;
    let updated_at: String = row
        .try_get("updated_at")
        .map_err(storage_err("updated_at"))?;
    let completed_at: Option<String> = row.try_get("completed_at").ok().flatten();

    let artifacts: Vec<ArtifactRef> = serde_json::from_str(&artifacts_json).unwrap_or_default();
    let events: Vec<DelegationEvent> = serde_json::from_str(&events_json).unwrap_or_default();

    Ok(DelegationTask {
        id,
        root_id,
        parent_id,
        depth: depth as u32,
        agent_id: agent_id.unwrap_or_default(),
        title: title.unwrap_or_default(),
        status,
        state_json,
        artifacts,
        events,
        created_at,
        updated_at,
        completed_at,
    })
}

fn storage_err(field: &'static str) -> impl Fn(sqlx::Error) -> crate::error::SyscityError {
    move |e| crate::error::SyscityError::Storage {
        context: format!("Failed to read delegation task column '{}'", field),
        details: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> DelegationTaskStore {
        DelegationTaskStore::new("sqlite::memory:")
            .await
            .expect("in-memory store")
    }

    /// Regression: a fresh install has no `delegations.db` file. sqlx 0.8
    /// defaults `create_if_missing` to false, so the store must create the
    /// file itself (mirrors `gateway/init/storage.rs`).
    #[tokio::test]
    async fn test_file_store_creates_missing_db() {
        let dir =
            std::env::temp_dir().join(format!("syscity-delegation-test-{}", std::process::id()));
        let db_path = dir.join("delegations.db");
        let url = format!("sqlite://{}", db_path.display());
        let store = DelegationTaskStore::new(&url).await.expect("file store");
        assert!(db_path.exists(), "store must create the database file");
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "T",
            })
            .await
            .expect("create task");
        let task = store.get_task("run-1").await.unwrap().expect("task exists");
        assert_eq!(task.status, "running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_create_and_get_task() {
        let store = test_store().await;
        let id = store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "Write a parser",
            })
            .await
            .unwrap();

        assert_eq!(id, "run-1");
        let task = store.get_task("run-1").await.unwrap().expect("task exists");
        assert_eq!(task.root_id, "root-1");
        assert_eq!(task.parent_id, None);
        assert_eq!(task.depth, 1);
        assert_eq!(task.status, "running");
        assert!(task.events.is_empty());
        assert!(task.artifacts.is_empty());
        assert_eq!(task.state().len(), 0);

        assert!(store.get_task("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_fail_orphaned_runs_sweeps_inflight_rows() {
        let store = test_store().await;
        store
            .create_task(NewTask {
                id: "run-orphan",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "Orphaned",
            })
            .await
            .unwrap();
        store
            .create_task(NewTask {
                id: "run-done",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "Done",
            })
            .await
            .unwrap();
        store.set_status("run-done", "completed").await.unwrap();

        let swept = store.fail_orphaned_runs().await.expect("sweep");
        assert_eq!(swept, 1, "only the in-flight row is swept");
        assert_eq!(store.get_task("run-orphan").await.unwrap().unwrap().status, "failed");
        assert_eq!(store.get_task("run-done").await.unwrap().unwrap().status, "completed");
        // Idempotent: a second sweep finds nothing.
        assert_eq!(store.fail_orphaned_runs().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_child_task_links_parent() {
        let store = test_store().await;
        store
            .create_task(NewTask {
                id: "parent",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "Plan",
            })
            .await
            .unwrap();
        store
            .create_task(NewTask {
                id: "child",
                root_id: "root-1",
                parent_id: Some("parent"),
                depth: 2,
                agent_id: "worker",
                title: "Do",
            })
            .await
            .unwrap();

        let all = store.tasks_for_root("root-1").await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "parent");
        assert_eq!(all[1].id, "child");
        assert_eq!(all[1].parent_id.as_deref(), Some("parent"));
    }

    #[tokio::test]
    async fn test_update_state_and_events() {
        let store = test_store().await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "T",
            })
            .await
            .unwrap();

        store
            .update_state("run-1", r#"{"progress": 0.5}"#)
            .await
            .unwrap();
        store
            .append_event("run-1", &DelegationEvent::new("coder", "set_state", "progress to 0.5"))
            .await
            .unwrap();

        let task = store.get_task("run-1").await.unwrap().unwrap();
        assert_eq!(task.state().get("progress"), Some(&serde_json::json!(0.5)));
        assert_eq!(task.events.len(), 1);
        assert_eq!(task.events[0].action, "set_state");
    }

    #[tokio::test]
    async fn test_add_artifact() {
        let store = test_store().await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "T",
            })
            .await
            .unwrap();

        store
            .add_artifact(
                "run-1",
                &ArtifactRef {
                    name: "report.md".to_string(),
                    url: "/api/v1/artifacts/report.md".to_string(),
                    size: 42,
                    producer: "coder".to_string(),
                },
            )
            .await
            .unwrap();

        let task = store.get_task("run-1").await.unwrap().unwrap();
        assert_eq!(task.artifacts.len(), 1);
        assert_eq!(task.artifacts[0].name, "report.md");
    }

    #[tokio::test]
    async fn test_status_and_completed_at() {
        let store = test_store().await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "T",
            })
            .await
            .unwrap();

        store.set_status("run-1", "completed").await.unwrap();
        let task = store.get_task("run-1").await.unwrap().unwrap();
        assert_eq!(task.status, "completed");
        assert!(task.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_handoff_and_pending_query() {
        let store = test_store().await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "T",
            })
            .await
            .unwrap();
        store
            .create_task(NewTask {
                id: "run-2",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "T2",
            })
            .await
            .unwrap();

        store
            .set_handoff("run-1", "reviewer", "needs review")
            .await
            .unwrap();

        let task = store.get_task("run-1").await.unwrap().unwrap();
        assert!(task.is_waiting_handoff());
        assert_eq!(task.agent_id, "reviewer");
        assert_eq!(task.events.len(), 1);
        assert_eq!(task.events[0].action, "handoff");

        // Oldest handoff is picked up first.
        let pending = store.pending_handoff_for_root("root-1").await.unwrap();
        assert_eq!(pending.unwrap().id, "run-1");
    }

    #[tokio::test]
    async fn test_no_pending_handoff() {
        let store = test_store().await;
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "T",
            })
            .await
            .unwrap();

        assert!(store
            .pending_handoff_for_root("root-1")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn test_missing_task_reads_are_empty() {
        let store = test_store().await;
        assert!(store.get_task("nope").await.unwrap().is_none());
        assert!(store.tasks_for_root("nope").await.unwrap().is_empty());
    }
}
