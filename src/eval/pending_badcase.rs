//! Pending badcase collection store.
//!
//! Badcases are auto-collected online from three sources before they enter the
//! RCA / regression pipeline:
//!
//! - `online:risk`  — deterministic `RiskSignalChecker` findings on a turn.
//! - `human:dislike` — a user's 👎 vote on a turn (`feedback.vote`).
//! - `computer:run` — a desktop run that stopped short of its goal
//!   (`need_help` / `budget` / `max_steps`), carrying the goal, the step that
//!   failed, the last capture and the action-ladder distribution.
//!
//! Rows are deduplicated by `sha256(input || '|' || response)`, so the same
//! failure is only queued once regardless of how many signals or votes fire on
//! it. An operator (or the auto pipeline) later confirms a row, which commits
//! it as a `BadcaseRecord` + YAML via [`recycle::write_badcase_yaml`], or
//! dismisses it.
//!
//! **Converting a row is not a formality.** A committed badcase becomes an
//! `EvalTask` in the regression suite, and a task with no `conditions` is a
//! task that always passes — a suite of those measures nothing. So
//! [`PendingBadcaseStore::confirm_and_commit`] refuses unless the caller
//! supplies an [`ExpectedEndState`]: the deterministic checks a *person*
//! confirmed against the state the run should have reached. The model's own
//! opinion of whether it finished is not evidence, which is why nothing here
//! derives conditions from a run's final message.

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Pool, Sqlite};
use tracing::{info, instrument};

use crate::error::{Result, SyscityError};
use crate::eval::loader::default_evals_dir;
use crate::goal::GoalCondition;

use super::dataset::{EvalTask, EvalTaskSource};
use super::rca::BadcaseEntry;
use super::recycle::{write_badcase_yaml, BadcaseFixStatus, BadcaseRecord};

/// Where a pending badcase came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSource {
    /// Deterministic online risk-signal scan of a completed turn.
    OnlineRisk,
    /// A human Like/Dislike vote (👎) on a turn.
    HumanDislike,
    /// A desktop run that stopped short of its goal.
    ///
    /// Deliberately *not* counted by the scalar optimizer's online-anomaly
    /// evaluator: that gate rolls back a change to the default agent's scalars
    /// (temperature, token budgets, concurrency), and a desktop run is driven by
    /// its own provider and loop configuration. A run failing is evidence about
    /// the desktop path, not about the scalar that was just applied — letting it
    /// trip the rollback would revert an unrelated change and teach the loop
    /// nothing.
    ComputerRun,
}

impl PendingSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            PendingSource::OnlineRisk => "online:risk",
            PendingSource::HumanDislike => "human:dislike",
            PendingSource::ComputerRun => "computer:run",
        }
    }
}

/// Lifecycle of a pending badcase row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingStatus {
    /// Collected, awaiting confirmation.
    Pending,
    /// Reviewed and accepted as a real badcase (pre-commit).
    Confirmed,
    /// Committed into a `BadcaseRecord` + YAML.
    Converted,
    /// Reviewed and rejected.
    Dismissed,
}

impl PendingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PendingStatus::Pending => "pending",
            PendingStatus::Confirmed => "confirmed",
            PendingStatus::Converted => "converted",
            PendingStatus::Dismissed => "dismissed",
        }
    }
}

/// A row in the pending badcase pool.
#[derive(Debug, Clone)]
pub struct PendingBadcase {
    pub id: String,
    pub source: PendingSource,
    pub turn_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub input: String,
    pub response: String,
    pub risk_signals: Vec<String>,
    /// Source-specific structured payload.
    ///
    /// Kept out of the dedup hash on purpose: a desktop run's last capture
    /// digest differs on every attempt, and folding it into the hash would make
    /// five repetitions of the same failure five rows. The hash keys on *what
    /// failed*; this column carries *the evidence from the attempt that first
    /// exhibited it*, which is what a person reads when deciding whether the row
    /// is a real badcase.
    pub detail: Option<serde_json::Value>,
    pub status: PendingStatus,
    pub created_at: i64,
}

/// One step of a failed desktop run, flattened for the badcase payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStepRecord {
    /// Zero-based step number, as the run numbered it.
    pub step: usize,
    /// The action in words (`click [s1:0.2]`), never a raw id alone.
    pub action: String,
    /// What the adapter said happened.
    pub result: String,
    /// `verified` / `failed` / `unverifiable`.
    pub verification: String,
    /// The rung of the action ladder the action was delivered at, if reported.
    pub rung: Option<String>,
}

/// A desktop run that stopped short, as the pool records it.
///
/// Deliberately built from plain data rather than `computer` types: the eval
/// layer owns this store, and the producer in the gateway is the adapter
/// between a finished run and this record. That keeps the conversion (goal,
/// failed step, capture, ladder distribution, handoff) unit-testable without a
/// desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFailureRecord {
    pub run_id: String,
    pub goal: String,
    /// `need_help` / `budget` / `max_steps` — the pool is only for runs that
    /// stopped short, so `done` and `cancelled` never reach here.
    pub stop_reason: String,
    pub steps_completed: usize,
    pub model_calls: usize,
    pub images_attached: usize,
    /// The last step whose verification failed, when there was one.
    pub failed_step: Option<RunStepRecord>,
    /// CAS digest of the run's final capture.
    pub last_screenshot_ref: Option<String>,
    /// How many steps were delivered at each rung, including `unknown`.
    pub rung_distribution: std::collections::BTreeMap<String, usize>,
    /// The handoff note (done / where / tried / next), as the person saw it.
    pub handoff: Option<serde_json::Value>,
    /// One line per step, in order, as the run log showed them.
    pub step_lines: Vec<String>,
}

impl RunFailureRecord {
    /// The dedup string: what failed, not which attempt saw it.
    ///
    /// Deterministic by construction — no run id, no timestamp, no capture
    /// digest — so the fifth repetition of the same dead end collapses onto the
    /// first row instead of flooding the pool. The free-text result is excluded
    /// for the same reason: it carries snapshot ids that change per observation.
    pub fn signature(&self) -> String {
        match &self.failed_step {
            Some(step) => format!(
                "{}: step {} — {} ({})",
                self.stop_reason,
                step.step + 1,
                step.action,
                step.verification
            ),
            None => format!(
                "{}: no failed step after {} completed step(s)",
                self.stop_reason, self.steps_completed
            ),
        }
    }

    /// The payload stored in the row's `detail` column.
    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({
            "run_id": self.run_id,
            "goal": self.goal,
            "stop_reason": self.stop_reason,
            "steps_completed": self.steps_completed,
            "model_calls": self.model_calls,
            "images_attached": self.images_attached,
            "failed_step": self.failed_step.as_ref().map(|s| serde_json::json!({
                "step": s.step,
                "action": s.action,
                "result": s.result,
                "verification": s.verification,
                "rung": s.rung,
            })),
            "last_screenshot_ref": self.last_screenshot_ref,
            "rung_distribution": self.rung_distribution,
            "handoff": self.handoff,
            "steps": self.step_lines,
        })
    }
}

/// What a *person* confirmed a passing run should have produced.
///
/// The whole point of a regression case is that it can fail. An `EvalTask` with
/// no conditions passes every trial (`conditions_passed` over an empty set is
/// `true`), so committing one would add a row to the suite that inflates the
/// pass rate and catches nothing. These checks must come from someone looking at
/// a run that worked, never from the model's own "done" message.
#[derive(Debug, Clone)]
pub struct ExpectedEndState {
    /// Who confirmed these checks (an operator name, a review id). Recorded in
    /// the committed YAML so a poisoned case is attributable.
    pub confirmed_by: String,
    /// Deterministic checks the end state must satisfy. Never empty.
    pub conditions: Vec<GoalCondition>,
    /// What a passing run looks like, in words, for the report.
    pub description: String,
}

impl ExpectedEndState {
    /// Build an expected end state, refusing the two shapes that cannot be a
    /// regression case: no confirmation, and nothing to check.
    pub fn new(
        confirmed_by: impl Into<String>,
        conditions: Vec<GoalCondition>,
        description: impl Into<String>,
    ) -> Result<Self> {
        let confirmed_by = confirmed_by.into();
        if confirmed_by.trim().is_empty() {
            return Err(SyscityError::Validation(
                "an expected end state must name who confirmed it".to_string(),
            ));
        }
        if conditions.is_empty() {
            return Err(SyscityError::Validation(
                "an expected end state needs at least one executable condition — \
                 a task without conditions passes unconditionally"
                    .to_string(),
            ));
        }
        Ok(Self {
            confirmed_by,
            conditions,
            description: description.into(),
        })
    }
}

/// Parameters for [`PendingBadcaseStore::insert_pending`].
#[derive(Debug, Clone)]
pub struct InsertPendingParams {
    pub source: PendingSource,
    pub turn_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub input: String,
    pub response: String,
    pub risk_signals: Vec<String>,
}

/// Compute the dedup hash for an input/response pair.
pub fn dedup_hash(input: &str, response: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher.update(b"|");
    hasher.update(response.as_bytes());
    hex::encode(hasher.finalize())
}

/// SQLite-backed store for the pending badcase pool.
#[derive(Debug, Clone)]
pub struct PendingBadcaseStore {
    pool: Pool<Sqlite>,
    /// Where a confirmed row's YAML is written. `None` = the project's
    /// `evals/badcases/`, which is what the gateway wants; a test pins a temp
    /// directory so committing a fixture never leaves a file in the repo.
    commit_dir: Option<PathBuf>,
}

impl PendingBadcaseStore {
    /// Create a new store from a database URL.
    #[instrument(skip(database_url))]
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(database_url)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to connect to pending badcase database".to_string(),
                details: e.to_string(),
            })?;
        Self::from_pool(pool).await
    }

    /// Create a store from an existing connection pool (shared with the other
    /// stores so all tables live in one database).
    pub async fn from_pool(pool: Pool<Sqlite>) -> Result<Self> {
        let store = Self { pool, commit_dir: None };
        store.init_schema().await?;
        info!("Pending badcase store initialized");
        Ok(store)
    }

    /// This store, committing confirmed badcases into `dir` instead of the
    /// project's `evals/badcases/`.
    pub fn with_commit_dir(self, dir: PathBuf) -> Self {
        Self { commit_dir: Some(dir), ..self }
    }

    async fn init_schema(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS pending_badcases (
                id            TEXT PRIMARY KEY,
                source        TEXT NOT NULL CHECK (source IN ('online:risk','human:dislike')),
                turn_id       TEXT,
                session_id    TEXT,
                agent_id      TEXT,
                input         TEXT NOT NULL,
                response      TEXT NOT NULL,
                risk_signals  TEXT NOT NULL DEFAULT '[]',
                detail        TEXT,
                dedup_hash    TEXT NOT NULL UNIQUE,
                status        TEXT NOT NULL CHECK (status IN ('pending','confirmed','converted','dismissed')),
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to create pending_badcases table".to_string(),
            details: e.to_string(),
        })?;
        self.migrate_schema().await
    }

    /// Bring a table created by an earlier version up to the current schema.
    ///
    /// Two kinds of drift, two mechanisms, because SQLite treats them
    /// differently:
    ///
    /// - **A new column** is an `ALTER TABLE ... ADD COLUMN`, guarded by
    ///   `PRAGMA table_info` (the idiom used for `memories`). Cheap and safe.
    /// - **A widened `CHECK`** is not alterable at all. The old table rejects
    ///   `computer:run` outright, so the only way forward is the documented
    ///   rebuild: create the new shape under a temporary name, copy, drop,
    ///   rename — inside one transaction, so a crash leaves the old table
    ///   intact rather than a half-copied one.
    async fn migrate_schema(&self) -> Result<()> {
        use sqlx::Row;

        let columns: std::collections::HashSet<String> =
            sqlx::query("PRAGMA table_info(pending_badcases)")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to inspect pending_badcases schema".to_string(),
                    details: e.to_string(),
                })?
                .iter()
                .map(|row| row.get::<String, _>("name"))
                .collect();

        if !columns.contains("detail") {
            sqlx::query("ALTER TABLE pending_badcases ADD COLUMN detail TEXT")
                .execute(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Schema migration failed for pending_badcases.detail".to_string(),
                    details: e.to_string(),
                })?;
            info!("Added column detail to pending_badcases table");
        }

        // Whether the table still carries the old, two-source CHECK. Read from
        // the stored DDL rather than from a version counter: the DDL is the
        // thing that actually decides whether an insert is rejected.
        let ddl: Option<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'pending_badcases'",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to read pending_badcases DDL".to_string(),
            details: e.to_string(),
        })?;

        let needs_rebuild = ddl
            .as_deref()
            .map(|sql| !sql.contains("'computer:run'"))
            .unwrap_or(false);
        if needs_rebuild {
            self.rebuild_for_computer_run_source().await?;
        }
        Ok(())
    }

    /// Rebuild `pending_badcases` with the widened source `CHECK`.
    ///
    /// One transaction: the copy, the drop and the rename all land together, so
    /// a process death mid-migration leaves the original table queryable.
    async fn rebuild_for_computer_run_source(&self) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|e| SyscityError::Storage {
            context: "Failed to begin pending_badcases migration".to_string(),
            details: e.to_string(),
        })?;

        let statements = [
            r#"
            CREATE TABLE pending_badcases_migrated (
                id            TEXT PRIMARY KEY,
                source        TEXT NOT NULL CHECK (source IN ('online:risk','human:dislike','computer:run')),
                turn_id       TEXT,
                session_id    TEXT,
                agent_id      TEXT,
                input         TEXT NOT NULL,
                response      TEXT NOT NULL,
                risk_signals  TEXT NOT NULL DEFAULT '[]',
                detail        TEXT,
                dedup_hash    TEXT NOT NULL UNIQUE,
                status        TEXT NOT NULL CHECK (status IN ('pending','confirmed','converted','dismissed')),
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL
            )
            "#,
            r#"
            INSERT INTO pending_badcases_migrated
                (id, source, turn_id, session_id, agent_id, input, response, risk_signals,
                 detail, dedup_hash, status, created_at, updated_at)
            SELECT id, source, turn_id, session_id, agent_id, input, response, risk_signals,
                   detail, dedup_hash, status, created_at, updated_at
            FROM pending_badcases
            "#,
            "DROP TABLE pending_badcases",
            "ALTER TABLE pending_badcases_migrated RENAME TO pending_badcases",
        ];

        for statement in statements {
            sqlx::query(statement)
                .execute(&mut *tx)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to widen pending_badcases source constraint".to_string(),
                    details: e.to_string(),
                })?;
        }

        tx.commit().await.map_err(|e| SyscityError::Storage {
            context: "Failed to commit pending_badcases migration".to_string(),
            details: e.to_string(),
        })?;
        info!("Migrated pending_badcases to allow the computer:run source");
        Ok(())
    }

    /// Insert a pending badcase unless the same input/response already exists.
    ///
    /// Returns `Ok(true)` when inserted, `Ok(false)` when deduplicated.
    pub async fn insert_pending(&self, params: &InsertPendingParams) -> Result<bool> {
        self.insert_pending_detailed(params, None).await
    }

    /// [`Self::insert_pending`] with a structured `detail` payload.
    ///
    /// The detail is stored beside the row, never hashed into it: two failures
    /// with the same signature are one badcase even though their captures and
    /// run ids differ.
    pub async fn insert_pending_detailed(
        &self,
        params: &InsertPendingParams,
        detail: Option<serde_json::Value>,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp_millis();
        let id = uuid::Uuid::new_v4().to_string();
        let hash = dedup_hash(&params.input, &params.response);
        let signals_json =
            serde_json::to_string(&params.risk_signals).map_err(|e| SyscityError::Storage {
                context: "Failed to serialize risk signals".to_string(),
                details: e.to_string(),
            })?;
        let detail_json = match &detail {
            Some(value) => {
                Some(serde_json::to_string(value).map_err(|e| SyscityError::Storage {
                    context: "Failed to serialize badcase detail".to_string(),
                    details: e.to_string(),
                })?)
            }
            None => None,
        };

        let result = sqlx::query(
            r#"
            INSERT INTO pending_badcases
                (id, source, turn_id, session_id, agent_id, input, response, risk_signals,
                 detail, dedup_hash, status, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11, ?11)
            ON CONFLICT (dedup_hash) DO NOTHING
            "#,
        )
        .bind(&id)
        .bind(params.source.as_str())
        .bind(params.turn_id.as_deref())
        .bind(params.session_id.as_deref())
        .bind(params.agent_id.as_deref())
        .bind(&params.input)
        .bind(&params.response)
        .bind(&signals_json)
        .bind(detail_json.as_deref())
        .bind(&hash)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to insert pending badcase".to_string(),
            details: e.to_string(),
        })?;

        Ok(result.rows_affected() > 0)
    }

    /// Record a desktop run that stopped short.
    ///
    /// The row's `input` is the goal and its `response` is the failure
    /// [`RunFailureRecord::signature`] — the pair that dedups repetitions of the
    /// same dead end. Everything about the individual attempt lives in `detail`.
    ///
    /// Returns `Ok(true)` when the pool learned something new.
    pub async fn insert_run_failure(
        &self,
        run: &RunFailureRecord,
        session_id: Option<String>,
    ) -> Result<bool> {
        let params = InsertPendingParams {
            source: PendingSource::ComputerRun,
            turn_id: Some(run.run_id.clone()),
            session_id,
            agent_id: None,
            input: run.goal.clone(),
            response: run.signature(),
            risk_signals: run_risk_signals(run),
        };
        self.insert_pending_detailed(&params, Some(run.detail()))
            .await
    }

    /// List pending rows by status, oldest first.
    pub async fn list_pending(
        &self,
        status: PendingStatus,
        limit: u32,
    ) -> Result<Vec<PendingBadcase>> {
        let rows = sqlx::query(
            r#"
            SELECT id, source, turn_id, session_id, agent_id, input, response, risk_signals,
                   detail, status, created_at
            FROM pending_badcases
            WHERE status = ?1
            ORDER BY created_at ASC
            LIMIT ?2
            "#,
        )
        .bind(status.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to list pending badcases".to_string(),
            details: e.to_string(),
        })?;

        rows.iter()
            .map(|row| {
                use sqlx::Row;
                let risk_signals_json: String = row.get("risk_signals");
                let risk_signals: Vec<String> =
                    serde_json::from_str(&risk_signals_json).unwrap_or_default();
                let detail_json: Option<String> = row.get("detail");
                let detail = detail_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok());
                Ok(PendingBadcase {
                    id: row.get("id"),
                    source: parse_source(row.get("source"))?,
                    turn_id: row.get("turn_id"),
                    session_id: row.get("session_id"),
                    agent_id: row.get("agent_id"),
                    input: row.get("input"),
                    response: row.get("response"),
                    risk_signals,
                    detail,
                    status: parse_status(row.get("status"))?,
                    created_at: row.get("created_at"),
                })
            })
            .collect()
    }

    /// Mark a pending row confirmed.
    pub async fn confirm(&self, id: &str) -> Result<()> {
        self.set_status(id, PendingStatus::Confirmed).await
    }

    /// Mark a pending row dismissed (false positive / duplicate).
    pub async fn dismiss(&self, id: &str) -> Result<()> {
        self.set_status(id, PendingStatus::Dismissed).await
    }

    /// Confirm a pending badcase and commit it into the badcase regression
    /// suite as a `BadcaseRecord` + YAML file.
    ///
    /// `expected` is the human-confirmed end state that becomes the task's
    /// `conditions`. It is required rather than optional because the failure
    /// mode this guards against is silent: `EvalHarness` treats an empty
    /// condition set as `conditions_passed = true`, so a badcase committed
    /// without one would join the regression suite as a task that can never
    /// fail. The suite's pass rate would look healthy and measure nothing.
    ///
    /// Returns the written YAML path. Idempotent: a row already `converted`
    /// returns the existing path without rewriting.
    pub async fn confirm_and_commit(
        &self,
        id: &str,
        expected: &ExpectedEndState,
    ) -> Result<PathBuf> {
        // Read through the same projection the listing uses, so a row and its
        // committed record cannot disagree about the source or the detail.
        let row = self.rows_by_id(id).await?;
        let Some(row) = row else {
            return Err(SyscityError::Storage {
                context: "Pending badcase not found".to_string(),
                details: format!("no row with id {id}"),
            });
        };

        // A row already committed can be confirmed again without a rewrite.
        if row.status == PendingStatus::Converted {
            let existing = self.commit_path(id);
            if existing.exists() {
                return Ok(existing);
            }
        }

        let output_dir = self.commit_dir();
        let task_id = format!("badcase-{}", &id[..8.min(id.len())]);
        let record = BadcaseRecord {
            id: format!("{task_id}_{}", &id[..8]),
            task_id: task_id.clone(),
            input: row.input.clone(),
            description: source_description(row.source),
            failure_reason: if row.risk_signals.is_empty() {
                source_description(row.source)
            } else {
                row.risk_signals.join("; ")
            },
            response: row.response.clone(),
            difficulty: "medium".into(),
            coverage: Vec::new(),
            rca_performed: false,
            rca_result: None,
            collected_at: std::time::SystemTime::now(),
            fix_status: BadcaseFixStatus::Unconfirmed,
            entry: badcase_entry_for(row.source),
        };

        let original = EvalTask {
            id: task_id,
            description: record.description.clone(),
            input: row.input,
            source: EvalTaskSource::BadcaseRecycle,
            failure_reason: Some(record.failure_reason.clone()),
            // The confirmed end state, not the model's own verdict: this is what
            // makes the committed task able to fail.
            conditions: expected.conditions.clone(),
            expected_behavior: expected.description.clone(),
            ..Default::default()
        };

        let path = write_badcase_yaml(&record, &original, &output_dir)?;
        self.set_status(id, PendingStatus::Converted).await?;
        info!(
            "Committed pending badcase {id} ({} condition(s), confirmed by {}) to {:?}",
            expected.conditions.len(),
            expected.confirmed_by,
            path
        );
        Ok(path)
    }

    /// One row by id, in the same shape [`Self::list_pending`] returns.
    pub async fn rows_by_id(&self, id: &str) -> Result<Option<PendingBadcase>> {
        let rows = sqlx::query(
            r#"
            SELECT id, source, turn_id, session_id, agent_id, input, response, risk_signals,
                   detail, status, created_at
            FROM pending_badcases WHERE id = ?1
            "#,
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to fetch pending badcase".to_string(),
            details: e.to_string(),
        })?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        use sqlx::Row;
        let risk_signals_json: String = row.get("risk_signals");
        let detail_json: Option<String> = row.get("detail");
        Ok(Some(PendingBadcase {
            id: row.get("id"),
            source: parse_source(row.get("source"))?,
            turn_id: row.get("turn_id"),
            session_id: row.get("session_id"),
            agent_id: row.get("agent_id"),
            input: row.get("input"),
            response: row.get("response"),
            risk_signals: serde_json::from_str(&risk_signals_json).unwrap_or_default(),
            detail: detail_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok()),
            status: parse_status(row.get("status"))?,
            created_at: row.get("created_at"),
        }))
    }

    async fn set_status(&self, id: &str, status: PendingStatus) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query("UPDATE pending_badcases SET status = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(status.as_str())
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to update pending badcase status".to_string(),
                details: e.to_string(),
            })?;
        Ok(())
    }

    /// Directory where committed badcase YAML files are written.
    fn commit_dir(&self) -> PathBuf {
        self.commit_dir
            .clone()
            .unwrap_or_else(|| default_evals_dir().join("badcases"))
    }

    /// The expected YAML path for a committed badcase (for idempotent reuse).
    fn commit_path(&self, id: &str) -> PathBuf {
        self.commit_dir()
            .join(format!("{}.yaml", &id[..8.min(id.len())]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> PendingBadcaseStore {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        PendingBadcaseStore::from_pool(pool).await.unwrap()
    }

    /// A store whose commits land in a temp directory the caller owns.
    ///
    /// Without this, `confirm_and_commit` writes into `evals/badcases/` of the
    /// repository — a test that passes on a developer's machine by editing a
    /// checked-in tree.
    async fn committing_store(dir: &std::path::Path) -> PendingBadcaseStore {
        test_store().await.with_commit_dir(dir.to_path_buf())
    }

    fn params(source: PendingSource, input: &str, response: &str) -> InsertPendingParams {
        InsertPendingParams {
            source,
            turn_id: Some("t1".into()),
            session_id: None,
            agent_id: None,
            input: input.to_string(),
            response: response.to_string(),
            risk_signals: vec!["PII detected".to_string()],
        }
    }

    #[tokio::test]
    async fn insert_is_deduplicated_on_input_response() {
        let store = test_store().await;
        let first = store
            .insert_pending(&params(PendingSource::OnlineRisk, "hello", "world"))
            .await
            .unwrap();
        assert!(first, "first insert should succeed");

        let second = store
            .insert_pending(&params(PendingSource::OnlineRisk, "hello", "world"))
            .await
            .unwrap();
        assert!(!second, "duplicate input|response should be rejected");

        // Same input, different response is a distinct badcase.
        let third = store
            .insert_pending(&params(PendingSource::OnlineRisk, "hello", "world2"))
            .await
            .unwrap();
        assert!(third);
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let store = test_store().await;
        store
            .insert_pending(&params(PendingSource::HumanDislike, "u1", "r1"))
            .await
            .unwrap();
        store
            .insert_pending(&params(PendingSource::OnlineRisk, "u2", "r2"))
            .await
            .unwrap();

        let pending = store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
        // Oldest-first: the HumanDislike row was inserted first.
        assert_eq!(pending[0].source, PendingSource::HumanDislike);
        assert_eq!(pending[1].source, PendingSource::OnlineRisk);
        assert_eq!(pending[0].risk_signals, vec!["PII detected"]);

        let converted = store
            .list_pending(PendingStatus::Converted, 10)
            .await
            .unwrap();
        assert!(converted.is_empty());
    }

    #[tokio::test]
    async fn dismiss_and_confirm_flip_status() {
        let store = test_store().await;
        store
            .insert_pending(&params(PendingSource::HumanDislike, "u1", "r1"))
            .await
            .unwrap();
        let id = store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap()[0]
            .id
            .clone();

        store.dismiss(&id).await.unwrap();
        assert!(store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .list_pending(PendingStatus::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1
        );

        store.confirm(&id).await.unwrap();
        assert_eq!(
            store
                .list_pending(PendingStatus::Confirmed, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn confirm_and_commit_writes_yaml() {
        let dir = tempfile::tempdir().expect("temp commit dir");
        let store = committing_store(dir.path()).await;
        store
            .insert_pending(&params(PendingSource::OnlineRisk, "u1", "r1"))
            .await
            .unwrap();
        let id = store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap()[0]
            .id
            .clone();

        let expected = ExpectedEndState::new(
            "ops-jane",
            vec![GoalCondition::FileExists {
                path: "/tmp/badcase-sentinel".to_string(),
            }],
            "the file exists after the run",
        )
        .expect("confirmed end state");

        let path = store.confirm_and_commit(&id, &expected).await.unwrap();
        assert!(path.exists(), "committed badcase YAML should exist");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("tasks"));
        assert!(content.contains("u1"));
        // The confirmed checks are in the YAML: without them the task passes
        // every trial and measures nothing.
        assert!(content.contains("file_exists"), "{content}");
        assert!(content.contains("/tmp/badcase-sentinel"), "{content}");

        // Row is now converted; committing again is idempotent.
        let path2 = store.confirm_and_commit(&id, &expected).await.unwrap();
        assert_eq!(path, path2);
        assert_eq!(
            store
                .list_pending(PendingStatus::Converted, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    // ── Stage D′: a failed desktop run becomes a regression case ─────────

    /// A run that stopped for help, with one failed step at the pixel rung.
    fn failed_run(run_id: &str) -> RunFailureRecord {
        let mut ladder = std::collections::BTreeMap::new();
        for rung in ["ax_action", "pixel", "keyboard", "page", "unknown"] {
            ladder.insert(rung.to_string(), 0usize);
        }
        ladder.insert("ax_action".to_string(), 2);
        ladder.insert("pixel".to_string(), 1);

        RunFailureRecord {
            run_id: run_id.to_string(),
            goal: "export the report as CSV".to_string(),
            stop_reason: "need_help".to_string(),
            steps_completed: 3,
            model_calls: 4,
            images_attached: 1,
            failed_step: Some(RunStepRecord {
                step: 2,
                action: "click [s1:0.4]".to_string(),
                result: "the element did not respond".to_string(),
                verification: "failed".to_string(),
                rung: Some("pixel".to_string()),
            }),
            last_screenshot_ref: Some("sha256:deadbeef".to_string()),
            rung_distribution: ladder,
            handoff: Some(serde_json::json!({
                "stuck_at": "step 3 — click [s1:0.4]",
                "next": "check the export dialog",
            })),
            step_lines: vec!["1. launch Numbers → ok".to_string()],
        }
    }

    /// The plan's first D′ check: a run that stops short leaves a row with
    /// every field a person needs to triage it — including the capture and the
    /// ladder distribution.
    #[tokio::test]
    async fn a_failed_run_leaves_one_complete_badcase() {
        let store = test_store().await;
        let run = failed_run("run-1");
        let inserted = store
            .insert_run_failure(&run, Some("sess-1".to_string()))
            .await
            .unwrap();
        assert!(inserted, "the first failure is queued");

        let rows = store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.source, PendingSource::ComputerRun);
        assert_eq!(row.input, "export the report as CSV");
        assert_eq!(row.turn_id.as_deref(), Some("run-1"));
        assert_eq!(row.session_id.as_deref(), Some("sess-1"));

        // The risk-signal lines say why it is here, in the run's own terms.
        assert!(
            row.risk_signals.iter().any(|s| s.contains("need_help")),
            "{:?}",
            row.risk_signals
        );
        assert!(row.risk_signals.iter().any(|s| s.contains("pixel")), "{:?}", row.risk_signals);

        // The structured payload carries the handoff, the failing step, the
        // last capture and the ladder distribution.
        let detail = row.detail.as_ref().expect("detail payload");
        assert_eq!(detail["goal"], "export the report as CSV");
        assert_eq!(detail["stop_reason"], "need_help");
        assert_eq!(detail["failed_step"]["step"], 2);
        assert_eq!(detail["failed_step"]["rung"], "pixel");
        assert_eq!(detail["last_screenshot_ref"], "sha256:deadbeef");
        assert_eq!(detail["rung_distribution"]["ax_action"], 2);
        assert_eq!(detail["rung_distribution"]["pixel"], 1);
        assert_eq!(detail["rung_distribution"]["page"], 0);
        assert_eq!(detail["handoff"]["stuck_at"], "step 3 — click [s1:0.4]");
        assert_eq!(detail["steps"][0], "1. launch Numbers → ok");
    }

    /// The plan's second check: repeating the same failure five times is one
    /// badcase, not five. The attempts differ only in things a person does not
    /// care about (run id, capture digest, result text) — the signature is what
    /// dedups.
    #[tokio::test]
    async fn repeated_identical_failures_collapse_to_one_row() {
        let store = test_store().await;
        for i in 0..5 {
            let mut run = failed_run(&format!("run-{i}"));
            // Every attempt looks different, and none of it should matter.
            run.last_screenshot_ref = Some(format!("sha256:{i:08x}"));
            if let Some(step) = run.failed_step.as_mut() {
                step.result = format!("the element did not respond (attempt {i})");
            }
            let inserted = store.insert_run_failure(&run, None).await.unwrap();
            assert_eq!(inserted, i == 0, "only the first attempt is a new row");
        }

        let rows = store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "five attempts, one badcase: {rows:?}");

        // The row that survives is the first attempt's evidence, not a merge.
        let detail = rows[0].detail.as_ref().expect("detail");
        assert_eq!(detail["run_id"], "run-0");
        assert_eq!(detail["last_screenshot_ref"], "sha256:00000000");
    }

    /// The plan's third check, and the one that keeps the regression set
    /// honest: a badcase cannot become an eval case without a human-confirmed
    /// end state. The refusal is at construction, so there is no API shape that
    /// reaches the YAML writer with an empty condition list.
    #[test]
    fn a_conversion_without_a_confirmed_end_state_is_refused() {
        // Nothing to check → no case.
        let empty = ExpectedEndState::new("ops-jane", Vec::new(), "it works");
        assert!(empty.is_err(), "a condition-less task would always pass");

        // Nobody confirmed it → no case.
        let unconfirmed = ExpectedEndState::new(
            "   ",
            vec![GoalCondition::FileExists { path: "/tmp/x".to_string() }],
            "the file exists",
        );
        assert!(unconfirmed.is_err(), "an end state needs a confirmant");

        // Both present → accepted.
        assert!(ExpectedEndState::new(
            "ops-jane",
            vec![GoalCondition::FileExists { path: "/tmp/x".to_string() }],
            "the file exists",
        )
        .is_ok());
    }

    /// A row committed with a confirmed end state writes conditions into the
    /// YAML the harness loads, so the regression case can actually fail.
    #[tokio::test]
    async fn a_confirmed_computer_run_badcase_lands_in_the_suite_with_conditions() {
        let dir = tempfile::tempdir().expect("temp commit dir");
        let badcases_dir = dir.path().join("badcases");
        let store = committing_store(&badcases_dir).await;
        store
            .insert_run_failure(&failed_run("run-1"), None)
            .await
            .unwrap();
        let id = store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap()[0]
            .id
            .clone();

        let expected = ExpectedEndState::new(
            "ops-jane",
            vec![GoalCondition::FileExists {
                path: "${trial_dir}/export.csv".to_string(),
            }],
            "an export.csv exists when the goal is met",
        )
        .expect("confirmed end state");
        store.confirm_and_commit(&id, &expected).await.unwrap();

        // The committed case is loadable by the suite the gate runs.
        let suite = crate::eval::recycle::load_badcase_suite(dir.path()).unwrap();
        assert_eq!(suite.tasks.len(), 1, "{suite:?}");
        let task = &suite.tasks[0];
        assert_eq!(task.input, "export the report as CSV");
        assert_eq!(task.conditions.len(), 1, "the case can fail: {task:?}");
        assert_eq!(task.expected_behavior, "an export.csv exists when the goal is met");
        assert_eq!(task.source, EvalTaskSource::BadcaseRecycle);
    }

    /// A database created before `computer:run` existed must accept the new
    /// source after migration — and keep its rows.
    #[tokio::test]
    async fn the_source_constraint_is_widened_for_existing_tables() {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        // The old shape, exactly: two sources, no `detail` column.
        sqlx::query(
            r#"
            CREATE TABLE pending_badcases (
                id            TEXT PRIMARY KEY,
                source        TEXT NOT NULL CHECK (source IN ('online:risk','human:dislike')),
                turn_id       TEXT,
                session_id    TEXT,
                agent_id      TEXT,
                input         TEXT NOT NULL,
                response      TEXT NOT NULL,
                risk_signals  TEXT NOT NULL DEFAULT '[]',
                dedup_hash    TEXT NOT NULL UNIQUE,
                status        TEXT NOT NULL CHECK (status IN ('pending','confirmed','converted','dismissed')),
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("old table");
        sqlx::query(
            r#"
            INSERT INTO pending_badcases
                (id, source, input, response, dedup_hash, status, created_at, updated_at)
            VALUES ('old-1', 'online:risk', 'u', 'r', 'hash-old', 'pending', 1, 1)
            "#,
        )
        .execute(&pool)
        .await
        .expect("pre-existing row");

        let store = PendingBadcaseStore::from_pool(pool).await.expect("migrate");

        // The pre-existing row survived the rebuild…
        let old = store
            .list_pending(PendingStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].id, "old-1");
        assert_eq!(old[0].source, PendingSource::OnlineRisk);

        // …and the new source is accepted.
        assert!(store
            .insert_run_failure(&failed_run("run-1"), None)
            .await
            .expect("computer:run is allowed after migration"));
    }
}

fn parse_source(s: String) -> Result<PendingSource> {
    match s.as_str() {
        "online:risk" => Ok(PendingSource::OnlineRisk),
        "human:dislike" => Ok(PendingSource::HumanDislike),
        "computer:run" => Ok(PendingSource::ComputerRun),
        other => Err(SyscityError::Storage {
            context: "Invalid source in pending_badcases".to_string(),
            details: format!("unexpected source: {other}"),
        }),
    }
}

fn parse_status(s: String) -> Result<PendingStatus> {
    match s.as_str() {
        "pending" => Ok(PendingStatus::Pending),
        "confirmed" => Ok(PendingStatus::Confirmed),
        "converted" => Ok(PendingStatus::Converted),
        "dismissed" => Ok(PendingStatus::Dismissed),
        other => Err(SyscityError::Storage {
            context: "Invalid status in pending_badcases".to_string(),
            details: format!("unexpected status: {other}"),
        }),
    }
}

fn source_description(source: PendingSource) -> String {
    match source {
        PendingSource::OnlineRisk => {
            "Auto-collected online: deterministic risk signal detected on a turn.".to_string()
        }
        PendingSource::HumanDislike => {
            "Auto-collected online: user disliked (👎) this turn.".to_string()
        }
        PendingSource::ComputerRun => {
            "Auto-collected online: a desktop run stopped short of its goal.".to_string()
        }
    }
}

fn badcase_entry_for(source: PendingSource) -> BadcaseEntry {
    match source {
        PendingSource::OnlineRisk => BadcaseEntry::OnlineRisk,
        PendingSource::HumanDislike => BadcaseEntry::HumanVote,
        PendingSource::ComputerRun => BadcaseEntry::ComputerRun,
    }
}

/// The risk-signal lines for a failed run.
///
/// These occupy the `risk_signals` column, which the dashboard and the
/// feedback report read as "what is wrong here". They are written from facts the
/// run recorded (why it stopped, which rung it settled on) rather than from a
/// model's prose about the failure.
fn run_risk_signals(run: &RunFailureRecord) -> Vec<String> {
    let mut signals = vec![format!("computer run stopped: {}", run.stop_reason)];
    if let Some(step) = &run.failed_step {
        signals.push(format!(
            "step {} failed ({}) — {}",
            step.step + 1,
            step.verification,
            step.action
        ));
        if let Some(rung) = &step.rung {
            signals.push(format!("failed at the {rung} rung of the action ladder"));
        } else {
            signals.push("the failed step reported no action-ladder evidence".to_string());
        }
    }
    if run.images_attached == 0 {
        signals.push("no capture was attached to any decision".to_string());
    }
    signals
}
