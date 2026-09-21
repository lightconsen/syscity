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
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, instrument, warn};

/// Prefix of a delegated task's own message session
/// (`delegation:<run_id>`); a `parent_session` starting with it names a
/// parent *run*, not a user-visible session, so root-session resolution must
/// climb to that parent's row.
pub const DELEGATION_SESSION_PREFIX: &str = "delegation:";

/// How many `parent_id` hops `root_session_for_task` will climb before
/// giving up. Normal trees are depth ≤ 3; the cap only guards a corrupt row
/// chain.
const MAX_SESSION_WALK_HOPS: usize = 8;

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
    /// Session the creating `delegate` call ran in: the user session for a
    /// tree root, `delegation:<parent_run_id>` for a delegated parent.
    /// `None` for rows written before the column existed.
    pub parent_session: Option<String>,
    /// Total tokens the task's LLM rounds have reported so far.
    pub usage_tokens: u64,
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

/// Wire projection of a delegation task row for push events.
///
/// Self-sufficient for rendering: a client can draw a task row from one
/// `delegation.updated` event without fetching the row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationTaskSnapshot {
    /// Task id (equal to the registry run id).
    pub task_id: String,
    /// Root of the delegation tree.
    pub root_id: String,
    /// Parent task id (`None` for a tree root).
    pub parent_id: Option<String>,
    /// Nesting depth (top-level delegation = 1).
    pub depth: u32,
    /// Agent id responsible for the task.
    pub agent_id: String,
    /// Short human title.
    pub title: String,
    /// Status: `pending | running | completed | failed | waiting_handoff`.
    pub status: String,
    /// RFC3339 creation time.
    pub created_at: String,
    /// RFC3339 last update time.
    pub updated_at: String,
    /// RFC3339 completion time (`None` while active).
    pub completed_at: Option<String>,
    /// Total tokens the task's LLM rounds have reported so far.
    pub usage_tokens: u64,
    /// `completed_at − created_at` when terminal, computed by the forwarder —
    /// clients should not parse RFC3339 to get an elapsed.
    pub duration_ms: Option<u64>,
}

impl DelegationTaskSnapshot {
    /// Project a full task row onto the wire shape. `duration_ms` is supplied
    /// by the caller (the forwarder, which parses the timestamps).
    pub fn from_task(task: &DelegationTask, duration_ms: Option<u64>) -> Self {
        Self {
            task_id: task.id.clone(),
            root_id: task.root_id.clone(),
            parent_id: task.parent_id.clone(),
            depth: task.depth,
            agent_id: task.agent_id.clone(),
            title: task.title.clone(),
            status: task.status.clone(),
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
            completed_at: task.completed_at.clone(),
            usage_tokens: task.usage_tokens,
            duration_ms,
        }
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
    /// Session the `delegate` call ran in: the user session for a tree root,
    /// `delegation:<parent_run_id>` for a delegated parent. Used to route
    /// push events to the client watching the root conversation.
    pub parent_session: Option<&'a str>,
}

/// SQLite-backed shared task state store for delegation trees.
///
/// Writes also feed an optional event sink (see [`Self::with_event_sink`]):
/// the gateway mounts one so task changes can be pushed to WS clients.
#[derive(Debug, Clone)]
pub struct DelegationTaskStore {
    pool: Pool<Sqlite>,
    event_sink: Option<UnboundedSender<String>>,
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

        let store = Self { pool, event_sink: None };
        store.init_schema().await?;
        Ok(store)
    }

    /// Attach the event sink task changes are reported through. The sink
    /// carries only the task id; its consumer (the gateway forwarder) re-reads
    /// the row, so every notification is delivered against fresh data.
    pub fn with_event_sink(mut self, tx: UnboundedSender<String>) -> Self {
        self.event_sink = Some(tx);
        self
    }

    /// Report a task change to the event sink, best effort. The only failure
    /// mode is a gone consumer, which is worth one warning and nothing more.
    fn notify_updated(&self, id: &str) {
        if let Some(tx) = &self.event_sink {
            if let Err(e) = tx.send(id.to_string()) {
                warn!("delegation event sink closed, task '{}' not reported: {}", id, e);
            }
        }
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

        // Migrate: parent_session / usage_tokens on databases created before
        // push events existed. CREATE TABLE IF NOT EXISTS won't touch an
        // existing file (same idiom as session_store/schema.rs).
        for (stmt, column) in [
            ("ALTER TABLE delegation_tasks ADD COLUMN parent_session TEXT", "parent_session"),
            (
                "ALTER TABLE delegation_tasks ADD COLUMN usage_tokens INTEGER NOT NULL DEFAULT 0",
                "usage_tokens",
            ),
        ] {
            if let Err(e) = sqlx::query(stmt).execute(&self.pool).await {
                if !e.to_string().contains("duplicate column name") {
                    warn!("Failed to add {} column to delegation_tasks: {}", column, e);
                }
            }
        }

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
                parent_session, status, created_at, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running', ?8, ?8)
            "#,
        )
        .bind(params.id)
        .bind(params.root_id)
        .bind(params.parent_id)
        .bind(params.depth as i64)
        .bind(params.agent_id)
        .bind(params.title)
        .bind(params.parent_session)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to create delegation task '{}'", params.id),
            details: e.to_string(),
        })?;
        self.notify_updated(params.id);

        Ok(params.id.to_string())
    }

    /// Add tokens one of the task's LLM rounds reported to the running total.
    /// Called per round from the child's progress callback, so the row total
    /// is live while the task runs.
    pub async fn add_usage(&self, id: &str, delta: u64) -> crate::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE delegation_tasks SET usage_tokens = usage_tokens + ?1, updated_at = ?2 \
             WHERE id = ?3",
        )
        .bind(delta as i64)
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| crate::error::SyscityError::Storage {
            context: format!("Failed to add usage to delegation task '{}'", id),
            details: e.to_string(),
        })?;
        self.notify_updated(id);
        Ok(())
    }

    /// The user-visible session a task's events should be routed to.
    ///
    /// A row's `parent_session` is the user session for a tree root and
    /// `delegation:<parent_run_id>` for a delegated parent, so a prefixed
    /// value means "climb to the parent row and ask again". Rows from before
    /// the column existed (and broken chains) resolve to `None`.
    pub async fn root_session_for_task(&self, id: &str) -> crate::Result<Option<String>> {
        let mut current_id = id.to_string();
        for _ in 0..MAX_SESSION_WALK_HOPS {
            let Some(task) = self.get_task(&current_id).await? else {
                return Ok(None);
            };
            if let Some(session) = &task.parent_session {
                if !session.starts_with(DELEGATION_SESSION_PREFIX) {
                    return Ok(Some(session.clone()));
                }
            }
            match task.parent_id {
                Some(parent) => current_id = parent,
                None => return Ok(None),
            }
        }
        warn!(
            "delegation task '{}' did not resolve to a root session within {} hops",
            id, MAX_SESSION_WALK_HOPS
        );
        Ok(None)
    }

    /// Mark tasks left in `running`/`waiting_handoff` by a previous process
    /// as failed. Called once at startup: those rows belong to executions
    /// that died with the last process and will never settle on their own.
    /// Returns the number of rows swept.
    ///
    /// Deliberately silent to the event sink: the sweep runs before any
    /// client can subscribe, and the bulk update has no ids to report.
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
    ///
    /// Column list is explicit (never `SELECT *`): the schema is extended by
    /// ALTER at startup, and a statement prepared against one shape must not
    /// drift onto another (session_store follows the same rule).
    pub async fn get_task(&self, id: &str) -> crate::Result<Option<DelegationTask>> {
        let row = sqlx::query(
            "SELECT id, root_id, parent_id, depth, agent_id, title, status, state_json, \
             artifacts_json, events_json, created_at, updated_at, completed_at, \
             parent_session, usage_tokens FROM delegation_tasks WHERE id = ?1",
        )
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
        self.notify_updated(id);
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
        self.notify_updated(id);
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
        self.notify_updated(id);
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
        self.notify_updated(id);
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
        self.notify_updated(id);
        Ok(())
    }

    /// Find the oldest `waiting_handoff` task under a root tree.
    pub async fn pending_handoff_for_root(
        &self,
        root_id: &str,
    ) -> crate::Result<Option<DelegationTask>> {
        let row = sqlx::query(
            r#"
            SELECT id, root_id, parent_id, depth, agent_id, title, status, state_json,
                   artifacts_json, events_json, created_at, updated_at, completed_at,
                   parent_session, usage_tokens
            FROM delegation_tasks
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
            "SELECT id, root_id, parent_id, depth, agent_id, title, status, state_json, \
             artifacts_json, events_json, created_at, updated_at, completed_at, \
             parent_session, usage_tokens FROM delegation_tasks \
             WHERE root_id = ?1 ORDER BY created_at ASC",
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
    let parent_session: Option<String> = row.try_get("parent_session").ok().flatten();
    let usage_tokens: i64 = row.try_get("usage_tokens").unwrap_or(0);

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
        parent_session,
        usage_tokens: usage_tokens.max(0) as u64,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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
                parent_session: None,
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

    /// A database written before `parent_session`/`usage_tokens` existed must
    /// reopen cleanly: the ALTER migrations add both columns, and rows from
    /// the old schema read with `None` / `0`.
    #[tokio::test]
    async fn test_legacy_schema_migrates() {
        let dir =
            std::env::temp_dir().join(format!("syscity-delegation-mig-{}", std::process::id()));
        // A previous run of this test may have failed before its cleanup,
        // leaving a truncated main file plus a stale WAL sidecar behind.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("delegations.db");
        let url = format!("sqlite://{}", db_path.display());

        // Old-shape table, written by hand: no parent_session, no usage_tokens.
        {
            // sqlx 0.8 defaults create_if_missing to false; the store creates
            // its own file, so the hand-rolled pool must too.
            std::fs::File::create(&db_path).unwrap();
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .unwrap();
            sqlx::query(
                r#"
                CREATE TABLE delegation_tasks (
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
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO delegation_tasks (id, root_id, status) VALUES ('old', 'r', 'running')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        // Reopening runs the migrations; the legacy row survives and reads.
        let store = DelegationTaskStore::new(&url)
            .await
            .expect("migrated store");
        let task = store.get_task("old").await.unwrap().expect("legacy row");
        assert_eq!(task.parent_session, None);
        assert_eq!(task.usage_tokens, 0);

        // New writes work on the migrated table.
        store
            .create_task(NewTask {
                id: "new",
                root_id: "r",
                parent_id: None,
                depth: 1,
                agent_id: "a",
                title: "T",
                parent_session: Some("sess-1"),
            })
            .await
            .unwrap();
        let new_task = store.get_task("new").await.unwrap().unwrap();
        assert_eq!(new_task.parent_session.as_deref(), Some("sess-1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh database carries the two columns without any migration work.
    #[tokio::test]
    async fn test_fresh_schema_has_push_columns() {
        let store = test_store().await;
        let row = sqlx::query(
            "SELECT COUNT(*) AS n FROM pragma_table_info('delegation_tasks') \
             WHERE name IN ('parent_session', 'usage_tokens')",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        let n: i64 = row.try_get("n").unwrap();
        assert_eq!(n, 2, "parent_session and usage_tokens must both exist");
    }

    #[tokio::test]
    async fn test_add_usage_accumulates_and_notifies() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let store = test_store().await.with_event_sink(tx);
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "T",
                parent_session: Some("sess-1"),
            })
            .await
            .unwrap();

        store.add_usage("run-1", 500).await.unwrap();
        store.add_usage("run-1", 300).await.unwrap();
        let task = store.get_task("run-1").await.unwrap().unwrap();
        assert_eq!(task.usage_tokens, 800);

        // create_task and both add_usage calls reported the id, in order.
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"));
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"));
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"));
    }

    /// Every row-writing method reports the task through the sink, so the
    /// gateway forwarder never needs a writer-side change.
    #[tokio::test]
    async fn test_every_write_notifies() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let store = test_store().await.with_event_sink(tx);
        store
            .create_task(NewTask {
                id: "run-1",
                root_id: "root-1",
                parent_id: None,
                depth: 1,
                agent_id: "coder",
                title: "T",
                parent_session: None,
            })
            .await
            .unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"), "create_task");

        store.update_state("run-1", "{}").await.unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"), "update_state");

        store
            .append_event("run-1", &DelegationEvent::new("a", "note", "d"))
            .await
            .unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"), "append_event");

        store
            .add_artifact(
                "run-1",
                &ArtifactRef {
                    name: "f.md".to_string(),
                    url: "/f.md".to_string(),
                    size: 1,
                    producer: "a".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"), "add_artifact");

        store.set_status("run-1", "waiting_handoff").await.unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"), "set_status");

        store.set_handoff("run-1", "reviewer", "s").await.unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("run-1"), "set_handoff");

        // The sink carries ids only; with no consumer the writes still succeed.
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        let silent = test_store().await.with_event_sink(tx2);
        silent
            .create_task(NewTask {
                id: "run-2",
                root_id: "r",
                parent_id: None,
                depth: 1,
                agent_id: "a",
                title: "T",
                parent_session: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_root_session_resolution_walks_the_parent_chain() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let store = test_store().await.with_event_sink(tx);

        // Depth-1 task: parent_session is the user session itself.
        store
            .create_task(NewTask {
                id: "root-task",
                root_id: "r",
                parent_id: None,
                depth: 1,
                agent_id: "manager",
                title: "Plan",
                parent_session: Some("user-session"),
            })
            .await
            .unwrap();
        // Depth-2 child: parent_session names the parent's delegated session.
        store
            .create_task(NewTask {
                id: "child",
                root_id: "r",
                parent_id: Some("root-task"),
                depth: 2,
                agent_id: "worker",
                title: "Do",
                parent_session: Some("delegation:root-task"),
            })
            .await
            .unwrap();
        // Depth-3 grandchild: one more hop up the chain.
        store
            .create_task(NewTask {
                id: "grandchild",
                root_id: "r",
                parent_id: Some("child"),
                depth: 3,
                agent_id: "helper",
                title: "Help",
                parent_session: Some("delegation:child"),
            })
            .await
            .unwrap();

        assert_eq!(
            store.root_session_for_task("root-task").await.unwrap(),
            Some("user-session".to_string())
        );
        assert_eq!(
            store.root_session_for_task("child").await.unwrap(),
            Some("user-session".to_string())
        );
        assert_eq!(
            store.root_session_for_task("grandchild").await.unwrap(),
            Some("user-session".to_string())
        );
        // Unknown id: nothing to resolve.
        assert_eq!(store.root_session_for_task("nope").await.unwrap(), None);

        // A broken chain (parent_id points nowhere) resolves to None.
        store
            .create_task(NewTask {
                id: "orphan",
                root_id: "r",
                parent_id: Some("vanished"),
                depth: 2,
                agent_id: "a",
                title: "T",
                parent_session: Some("delegation:vanished"),
            })
            .await
            .unwrap();
        assert_eq!(store.root_session_for_task("orphan").await.unwrap(), None);
    }
}
