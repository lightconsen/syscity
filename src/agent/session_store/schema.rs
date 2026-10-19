//! Schema initialization for the SQLite session store.
//!
//! Owns connection setup (`new` / `from_pool`), PRAGMA tuning, and the full
//! DDL + migration logic for every table the store uses.

use std::sync::Arc;
use std::time::Duration;

use sqlx::{sqlite::SqlitePoolOptions, Pool, Sqlite};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, warn};

use crate::error::{Result, SyscityError};

use super::SessionStore;

impl SessionStore {
    /// Create a new session store from a database URL.
    #[instrument(skip(database_url))]
    pub async fn new(database_url: &str) -> Result<Self> {
        info!("Initializing SQLite session store");

        // An in-memory database is opened with SQLite's shared cache, where
        // concurrent writers take table-level locks; sqlx 0.9 surfaces that as
        // SQLITE_LOCKED rather than waiting. One connection serialises them,
        // which is the only shape an in-memory store can keep.
        let in_memory = database_url.contains(":memory:");
        let (max_connections, min_connections) = if in_memory { (1, 1) } else { (10, 2) };

        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .min_connections(min_connections)
            .acquire_timeout(Duration::from_secs(30))
            .idle_timeout(Duration::from_secs(600))
            .max_lifetime(Duration::from_secs(3600))
            .connect(database_url)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to connect to database".to_string(),
                details: e.to_string(),
            })?;

        Self::from_pool(pool).await
    }

    /// Create a session store from an existing connection pool.
    pub async fn from_pool(pool: Pool<Sqlite>) -> Result<Self> {
        let store = Self {
            pool,
            cache: Arc::new(RwLock::new(lru::LruCache::new(
                #[allow(clippy::expect_used)] // 1000 is a known non-zero literal
                std::num::NonZeroUsize::new(1000).expect("1000 is non-zero"),
            ))),
        };

        store.optimize().await?;
        store.init_schema().await?;

        info!("SQLite session store initialized from pool");
        Ok(store)
    }

    /// Apply SQLite optimizations
    async fn optimize(&self) -> Result<()> {
        debug!("Applying database optimizations");

        // Enable WAL mode for better concurrency
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to enable WAL mode".to_string(),
                details: e.to_string(),
            })?;

        // Enable foreign keys
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to enable foreign keys".to_string(),
                details: e.to_string(),
            })?;

        // Set synchronous mode to NORMAL for better performance
        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to set synchronous mode".to_string(),
                details: e.to_string(),
            })?;

        Ok(())
    }

    /// Initialize database schema
    async fn init_schema(&self) -> Result<()> {
        debug!("Creating session storage schema");

        // Sessions table - stores session metadata and state
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_activity INTEGER NOT NULL,
                is_active INTEGER NOT NULL DEFAULT 1,
                pinned INTEGER NOT NULL DEFAULT 0,
                state_json TEXT,
                message_count INTEGER NOT NULL DEFAULT 0,
                name TEXT,
                bound_agent_id TEXT,
                transcript_id TEXT,
                model TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create sessions table".to_string(),
            details: e.to_string(),
        })?;

        // Migrate: add name column if it doesn't exist (for existing databases)
        if let Err(e) = sqlx::query("ALTER TABLE sessions ADD COLUMN name TEXT")
            .execute(&self.pool)
            .await
        {
            if !e.to_string().contains("duplicate column name") {
                warn!("Failed to add name column to sessions: {}", e);
            }
        }

        // Migrate: add bound_agent_id column if it doesn't exist
        if let Err(e) = sqlx::query("ALTER TABLE sessions ADD COLUMN bound_agent_id TEXT")
            .execute(&self.pool)
            .await
        {
            if !e.to_string().contains("duplicate column name") {
                warn!("Failed to add bound_agent_id column to sessions: {}", e);
            }
        }

        // Migrate: add pinned column if it doesn't exist
        if let Err(e) =
            sqlx::query("ALTER TABLE sessions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0")
                .execute(&self.pool)
                .await
        {
            if !e.to_string().contains("duplicate column name") {
                warn!("Failed to add pinned column to sessions: {}", e);
            }
        }

        // Migrate: add transcript_id column if it doesn't exist
        if let Err(e) = sqlx::query("ALTER TABLE sessions ADD COLUMN transcript_id TEXT")
            .execute(&self.pool)
            .await
        {
            if !e.to_string().contains("duplicate column name") {
                warn!("Failed to add transcript_id column to sessions: {}", e);
            }
        }

        // Migrate: add model column if it doesn't exist
        if let Err(e) = sqlx::query("ALTER TABLE sessions ADD COLUMN model TEXT")
            .execute(&self.pool)
            .await
        {
            if !e.to_string().contains("duplicate column name") {
                warn!("Failed to add model column to sessions: {}", e);
            }
        }

        // Session messages table - stores conversation history
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS session_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                reasoning_content TEXT,
                tool_calls_json TEXT,
                created_at INTEGER NOT NULL,
                metadata TEXT,
                transcript_id TEXT,
                run_id TEXT,
                turn_id TEXT,
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create messages table".to_string(),
            details: e.to_string(),
        })?;

        // ── Migration: add missing columns to existing session_messages tables
        // CREATE TABLE IF NOT EXISTS won't add columns to existing tables
        for col in &["transcript_id", "run_id", "turn_id"] {
            let result = sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE session_messages ADD COLUMN {} TEXT",
                col
            )))
            .execute(&self.pool)
            .await;
            if let Err(ref e) = result {
                // "duplicate column name" is expected — ignore it
                if !e.to_string().contains("duplicate column name") {
                    warn!("Failed to add column '{}' to session_messages: {}", col, e);
                }
            }
        }

        // Indexes for common queries
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_agent ON sessions(agent_id)")
            .execute(&self.pool)
            .await
            .map_err(|e| warn!("Failed to create session store index idx_sessions_agent: {}", e))
            .ok();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_sessions_channel ON sessions(channel, channel_id)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| warn!("Failed to create session store index idx_sessions_channel: {}", e))
        .ok();

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_activity ON sessions(last_activity)")
            .execute(&self.pool)
            .await
            .map_err(|e| warn!("Failed to create session store index idx_sessions_activity: {}", e))
            .ok();

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_sessions_pinned ON sessions(pinned)")
            .execute(&self.pool)
            .await
            .map_err(|e| warn!("Failed to create session store index idx_sessions_pinned: {}", e))
            .ok();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_session ON session_messages(session_id, \
             created_at)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| warn!("Failed to create session store index idx_messages_session: {}", e))
        .ok();

        // ── Thread / Turn additions ───────────────────────────────────────────

        // Threads table: one row per named conversation branch.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS threads (
                id          TEXT    NOT NULL,
                session_id  TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                label       TEXT    NOT NULL DEFAULT '',
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (id, session_id)
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create threads table".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_threads_session ON threads(session_id)")
            .execute(&self.pool)
            .await
            .map_err(|e| warn!("Failed to create session store index idx_threads_session: {}", e))
            .ok();

        // Migrate existing session_messages rows: add thread_id, turn_index,
        // turn_state columns if they are not already present.
        // SQLite does not support ADD COLUMN IF NOT EXISTS, so we check
        // pragma_table_info first.
        let has_thread_id: bool = match sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('session_messages') WHERE name='thread_id'",
        )
        .fetch_one(&self.pool)
        .await
        {
            Ok(count) => count > 0,
            Err(e) => {
                warn!("Failed to check pragma_table_info for thread_id: {}", e);
                // Default to true (already migrated) to avoid double ALTER
                true
            }
        };

        if !has_thread_id {
            sqlx::query("ALTER TABLE session_messages ADD COLUMN thread_id   TEXT")
                .execute(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to add thread_id column".to_string(),
                    details: e.to_string(),
                })?;
            sqlx::query("ALTER TABLE session_messages ADD COLUMN turn_index  INTEGER")
                .execute(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to add turn_index column".to_string(),
                    details: e.to_string(),
                })?;
            sqlx::query("ALTER TABLE session_messages ADD COLUMN turn_state  TEXT")
                .execute(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to add turn_state column".to_string(),
                    details: e.to_string(),
                })?;
            debug!("Migrated session_messages: added thread_id, turn_index, turn_state columns");
        }

        // Migrate: add reasoning_content and tool_calls_json columns if missing
        let has_reasoning: bool = match sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_info('session_messages') WHERE \
             name='reasoning_content'",
        )
        .fetch_one(&self.pool)
        .await
        {
            Ok(count) => count > 0,
            Err(e) => {
                warn!("Failed to check pragma_table_info for reasoning_content: {}", e);
                true
            }
        };

        if !has_reasoning {
            sqlx::query("ALTER TABLE session_messages ADD COLUMN reasoning_content TEXT")
                .execute(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to add reasoning_content column".to_string(),
                    details: e.to_string(),
                })?;
            sqlx::query("ALTER TABLE session_messages ADD COLUMN tool_calls_json TEXT")
                .execute(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to add tool_calls_json column".to_string(),
                    details: e.to_string(),
                })?;
            debug!("Migrated session_messages: added reasoning_content, tool_calls_json columns");
        }

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_thread ON session_messages(session_id, \
             thread_id, turn_index)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| warn!("Failed to create index idx_messages_thread: {}", e))
        .ok();

        // ── Subagent run records ──────────────────────────────────────────────

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS subagent_runs (
                run_id        TEXT PRIMARY KEY,
                subagent_id   TEXT NOT NULL,
                session_id    TEXT NOT NULL,
                parent_id     TEXT NOT NULL,
                label         TEXT,
                task_prompt   TEXT,
                mode          TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'starting',
                thread_id     TEXT,
                created_at    INTEGER NOT NULL,
                completed_at  INTEGER,
                result        TEXT,
                error         TEXT,
                killed_by     TEXT,
                steer_history TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create subagent_runs table".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_subagent_runs_session ON subagent_runs(session_id, \
             created_at)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| warn!("Failed to create index idx_subagent_runs_session: {}", e))
        .ok();

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_subagent_runs_subagent ON subagent_runs(subagent_id)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| warn!("Failed to create index idx_subagent_runs_subagent: {}", e))
        .ok();

        // ── ACP session records ───────────────────────────────────────────────

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS acp_sessions (
                session_id      TEXT PRIMARY KEY,
                parent_id       TEXT NOT NULL,
                subagent_ids    TEXT NOT NULL DEFAULT '[]',
                created_at      INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create acp_sessions table".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acp_sessions_parent ON acp_sessions(parent_id)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| warn!("Failed to create index idx_acp_sessions_parent: {}", e))
        .ok();

        // ── Observability metric tables ──────────────────────────────────────
        // Numeric per-turn facts backing `syscity observe stats` / `prune`.
        // One row per LLM call.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS llm_calls (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id              TEXT NOT NULL,
                session_id           TEXT,
                agent_id             TEXT,
                round                INTEGER NOT NULL,
                provider             TEXT NOT NULL,
                model                TEXT NOT NULL,
                started_at           INTEGER NOT NULL,
                duration_ms          INTEGER NOT NULL,
                ttft_ms              INTEGER,
                prompt_tokens        INTEGER NOT NULL DEFAULT 0,
                completion_tokens    INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens    INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                finish_reason        TEXT,
                error                TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create llm_calls table".to_string(),
            details: e.to_string(),
        })?;

        // One row per tool call.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS tool_call_metrics (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                turn_id     TEXT NOT NULL,
                session_id  TEXT,
                round       INTEGER NOT NULL,
                name        TEXT NOT NULL,
                started_at  INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                success     INTEGER NOT NULL,
                error       TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create tool_call_metrics table".to_string(),
            details: e.to_string(),
        })?;

        // One row per turn (small; drives the Turns block).
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS turn_outcomes (
                turn_id       TEXT PRIMARY KEY,
                session_id    TEXT,
                agent_id      TEXT,
                model         TEXT,
                started_at    INTEGER NOT NULL,
                queue_wait_ms INTEGER,
                duration_ms   INTEGER NOT NULL,
                ttft_ms       INTEGER,
                llm_rounds    INTEGER NOT NULL DEFAULT 0,
                tool_calls    INTEGER NOT NULL DEFAULT 0,
                cache_hit     INTEGER NOT NULL DEFAULT 0,
                state         TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create turn_outcomes table".to_string(),
            details: e.to_string(),
        })?;

        // ── Per-request header snapshots ─────────────────────────────────────
        // Debugging side table: one row per outgoing LLM request recording the
        // resolved model id, system prompt, and offered tool names/schemas.
        // Kept out of `session_messages` (rows can be large); pruned together
        // with the observability tables by `delete_metrics_before`.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS request_snapshots (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id      TEXT,
                conversation_id TEXT,
                agent_id        TEXT,
                model           TEXT NOT NULL,
                system_prompt   TEXT NOT NULL,
                tools_json      TEXT NOT NULL,
                created_at      INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create request_snapshots table".to_string(),
            details: e.to_string(),
        })?;

        for (table, index, cols) in [
            ("llm_calls", "idx_llm_calls_started", "started_at"),
            ("llm_calls", "idx_llm_calls_session", "session_id"),
            ("llm_calls", "idx_llm_calls_model", "model"),
            ("llm_calls", "idx_llm_calls_turn", "turn_id"),
            ("llm_calls", "idx_llm_calls_provider", "provider"),
            ("tool_call_metrics", "idx_tool_metrics_started", "started_at"),
            ("tool_call_metrics", "idx_tool_metrics_name", "name"),
            ("tool_call_metrics", "idx_tool_metrics_turn", "turn_id"),
            ("turn_outcomes", "idx_turn_outcomes_started", "started_at"),
            ("turn_outcomes", "idx_turn_outcomes_session", "session_id"),
            ("turn_outcomes", "idx_turn_outcomes_state", "state"),
            ("request_snapshots", "idx_request_snapshots_created", "created_at"),
            ("request_snapshots", "idx_request_snapshots_session", "session_id"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE INDEX IF NOT EXISTS {} ON {}({})",
                index, table, cols
            )))
            .execute(&self.pool)
            .await
            .map_err(|e| warn!("Failed to create session store index {}: {}", index, e))
            .ok();
        }

        info!("Session storage schema initialized");
        Ok(())
    }
}
