//! Subagent run records.
//!
//! Persists `subagent_runs` rows and exposes the [`SubagentRunRecord`] model.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::{debug, instrument};

use crate::error::{Result, SyscityError};

use super::{SaveSubagentRunParams, SessionStore};

/// Persisted subagent run record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRunRecord {
    pub run_id: String,
    pub subagent_id: String,
    pub session_id: String,
    pub parent_id: String,
    pub label: Option<String>,
    pub task_prompt: Option<String>,
    pub mode: String,
    pub status: String,
    pub thread_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub killed_by: Option<String>,
    pub steer_history: Option<Vec<String>>,
}

impl SessionStore {
    /// Save a subagent run record when it is spawned.
    #[instrument(skip(self))]
    pub async fn save_subagent_run(&self, params: &SaveSubagentRunParams<'_>) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let steer_json: Option<String> = Some("[]".to_string());

        sqlx::query(
            r#"
            INSERT INTO subagent_runs (run_id, subagent_id, session_id, parent_id, label, task_prompt, mode, status, thread_id, created_at, steer_history)
            VALUES (?, ?, ?, ?, ?, ?, ?, 'starting', ?, ?, ?)
            "#,
        )
        .bind(params.run_id)
        .bind(params.subagent_id)
        .bind(params.session_id)
        .bind(params.parent_id)
        .bind(params.label)
        .bind(params.task_prompt)
        .bind(params.mode)
        .bind(params.thread_id)
        .bind(now)
        .bind(steer_json)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to save subagent run".to_string(),
            details: e.to_string(),
        })?;

        debug!("Subagent run saved: {} (subagent={})", params.run_id, params.subagent_id);
        Ok(())
    }

    /// Update subagent run status (e.g. ready, busy, terminated, crashed).
    #[instrument(skip(self))]
    pub async fn update_subagent_run_status(&self, run_id: &str, status: &str) -> Result<()> {
        sqlx::query("UPDATE subagent_runs SET status = ? WHERE run_id = ?")
            .bind(status)
            .bind(run_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to update subagent run status".to_string(),
                details: e.to_string(),
            })?;

        debug!("Subagent run {} status updated to {}", run_id, status);
        Ok(())
    }

    /// Mark a subagent run as completed with result or error.
    #[instrument(skip(self))]
    pub async fn complete_subagent_run(
        &self,
        run_id: &str,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();

        sqlx::query(
            "UPDATE subagent_runs SET status = 'terminated', completed_at = ?, result = ?, error \
             = ? WHERE run_id = ?",
        )
        .bind(now)
        .bind(result)
        .bind(error)
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to complete subagent run".to_string(),
            details: e.to_string(),
        })?;

        debug!("Subagent run {} completed", run_id);
        Ok(())
    }

    /// Record a kill event on a subagent run.
    #[instrument(skip(self))]
    pub async fn kill_subagent_run(&self, run_id: &str, killed_by: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();

        sqlx::query(
            "UPDATE subagent_runs SET status = 'terminated', completed_at = ?, killed_by = ? \
             WHERE run_id = ?",
        )
        .bind(now)
        .bind(killed_by)
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to kill subagent run".to_string(),
            details: e.to_string(),
        })?;

        debug!("Subagent run {} killed by {}", run_id, killed_by);
        Ok(())
    }

    /// Append a steer event to a subagent run.
    #[instrument(skip(self))]
    pub async fn append_steer_to_run(&self, run_id: &str, steer_message: &str) -> Result<()> {
        let row = sqlx::query("SELECT steer_history FROM subagent_runs WHERE run_id = ?")
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to fetch steer_history".to_string(),
                details: e.to_string(),
            })?;

        let mut history: Vec<String> = if let Some(r) = row {
            let raw: Option<String> = r.get("steer_history");
            raw.and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            return Ok(());
        };

        history.push(steer_message.to_string());
        let updated = serde_json::to_string(&history).unwrap_or_else(|_| "[]".to_string());

        sqlx::query("UPDATE subagent_runs SET steer_history = ? WHERE run_id = ?")
            .bind(updated)
            .bind(run_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to update steer_history".to_string(),
                details: e.to_string(),
            })?;

        debug!("Steer appended to subagent run {}", run_id);
        Ok(())
    }

    /// Get a single subagent run by run_id.
    #[instrument(skip(self))]
    pub async fn get_subagent_run(&self, run_id: &str) -> Result<Option<SubagentRunRecord>> {
        let row = sqlx::query(
            r#"
            SELECT run_id, subagent_id, session_id, parent_id, label, task_prompt, mode, status,
                   thread_id, created_at, completed_at, result, error, killed_by, steer_history
            FROM subagent_runs
            WHERE run_id = ?
            "#,
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to get subagent run".to_string(),
            details: e.to_string(),
        })?;

        Ok(row.map(|r| Self::row_to_subagent_run_record(&r)))
    }

    /// List subagent runs for a session, ordered newest first.
    #[instrument(skip(self))]
    pub async fn list_subagent_runs(
        &self,
        session_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SubagentRunRecord>> {
        let mut query = String::from(
            r#"
            SELECT run_id, subagent_id, session_id, parent_id, label, task_prompt, mode, status,
                   thread_id, created_at, completed_at, result, error, killed_by, steer_history
            FROM subagent_runs
            WHERE 1=1
            "#,
        );

        if session_id.is_some() {
            query.push_str(" AND session_id = ?");
        }
        query.push_str(" ORDER BY created_at DESC LIMIT ?");

        let mut sql = sqlx::query(sqlx::AssertSqlSafe(query.as_str()));
        if let Some(sid) = session_id {
            sql = sql.bind(sid);
        }
        sql = sql.bind(limit);

        let rows = sql
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to list subagent runs".to_string(),
                details: e.to_string(),
            })?;

        Ok(rows.iter().map(Self::row_to_subagent_run_record).collect())
    }

    fn row_to_subagent_run_record(row: &sqlx::sqlite::SqliteRow) -> SubagentRunRecord {
        let steer_history: Option<Vec<String>> = row
            .get::<Option<String>, _>("steer_history")
            .and_then(|s| serde_json::from_str(&s).ok());

        SubagentRunRecord {
            run_id: row.get("run_id"),
            subagent_id: row.get("subagent_id"),
            session_id: row.get("session_id"),
            parent_id: row.get("parent_id"),
            label: row.get("label"),
            task_prompt: row.get("task_prompt"),
            mode: row.get("mode"),
            status: row.get("status"),
            thread_id: row.get("thread_id"),
            created_at: DateTime::from_timestamp_millis(row.get::<i64, _>("created_at"))
                .unwrap_or_else(Utc::now),
            completed_at: row
                .get::<Option<i64>, _>("completed_at")
                .and_then(DateTime::from_timestamp_millis),
            result: row.get("result"),
            error: row.get("error"),
            killed_by: row.get("killed_by"),
            steer_history,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_store() -> SessionStore {
        // Use in-memory SQLite for tests
        SessionStore::new(":memory:")
            .await
            .expect("Failed to create test store")
    }

    #[tokio::test]
    async fn test_subagent_run_lifecycle() {
        let store = create_test_store().await;

        // Save a run
        store
            .save_subagent_run(&SaveSubagentRunParams {
                run_id: "run-1",
                subagent_id: "subagent-1",
                session_id: "session-1",
                parent_id: "parent-1",
                label: Some("test label"),
                task_prompt: Some("do something"),
                mode: "run",
                thread_id: Some("thread-1"),
            })
            .await
            .unwrap();

        // Load it back
        let run = store.get_subagent_run("run-1").await.unwrap().unwrap();
        assert_eq!(run.run_id, "run-1");
        assert_eq!(run.subagent_id, "subagent-1");
        assert_eq!(run.session_id, "session-1");
        assert_eq!(run.parent_id, "parent-1");
        assert_eq!(run.label.as_deref(), Some("test label"));
        assert_eq!(run.task_prompt.as_deref(), Some("do something"));
        assert_eq!(run.mode, "run");
        assert_eq!(run.status, "starting");
        assert_eq!(run.thread_id.as_deref(), Some("thread-1"));
        assert!(run.steer_history.as_ref().unwrap().is_empty());

        // Update status
        store
            .update_subagent_run_status("run-1", "busy")
            .await
            .unwrap();
        let run2 = store.get_subagent_run("run-1").await.unwrap().unwrap();
        assert_eq!(run2.status, "busy");

        // Append steer
        store
            .append_steer_to_run("run-1", "change direction")
            .await
            .unwrap();
        let run3 = store.get_subagent_run("run-1").await.unwrap().unwrap();
        assert_eq!(run3.steer_history.as_ref().unwrap().len(), 1);
        assert_eq!(run3.steer_history.as_ref().unwrap()[0], "change direction");

        // Kill
        store.kill_subagent_run("run-1", "user").await.unwrap();
        let run4 = store.get_subagent_run("run-1").await.unwrap().unwrap();
        assert_eq!(run4.status, "terminated");
        assert_eq!(run4.killed_by.as_deref(), Some("user"));
        assert!(run4.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_subagent_run_complete() {
        let store = create_test_store().await;

        store
            .save_subagent_run(&SaveSubagentRunParams {
                run_id: "run-2",
                subagent_id: "subagent-2",
                session_id: "session-2",
                parent_id: "parent-2",
                label: None,
                task_prompt: None,
                mode: "session",
                thread_id: None,
            })
            .await
            .unwrap();

        store
            .complete_subagent_run("run-2", Some("all done"), None)
            .await
            .unwrap();

        let run = store.get_subagent_run("run-2").await.unwrap().unwrap();
        assert_eq!(run.status, "terminated");
        assert_eq!(run.result.as_deref(), Some("all done"));
        assert!(run.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_list_subagent_runs() {
        let store = create_test_store().await;

        for i in 0..3 {
            store
                .save_subagent_run(&SaveSubagentRunParams {
                    run_id: &format!("run-{}", i),
                    subagent_id: &format!("subagent-{}", i),
                    session_id: "session-a",
                    parent_id: "parent",
                    label: None,
                    task_prompt: None,
                    mode: "run",
                    thread_id: None,
                })
                .await
                .unwrap();
        }

        let runs = store
            .list_subagent_runs(Some("session-a"), 10)
            .await
            .unwrap();
        assert_eq!(runs.len(), 3);

        let limited = store
            .list_subagent_runs(Some("session-a"), 2)
            .await
            .unwrap();
        assert_eq!(limited.len(), 2);

        let other = store
            .list_subagent_runs(Some("session-b"), 10)
            .await
            .unwrap();
        assert!(other.is_empty());
    }
}
