//! Thread / turn persistence and session statistics.
//!
//! Upserts `threads` rows, appends completed turns (user + assistant rows),
//! loads threads with their turns, deletes a turn, and reports session stats.

use chrono::Utc;
use sqlx::Row;
use tracing::debug;

use crate::error::{Result, SyscityError};

use super::{SessionStats, SessionStore};

/// A persisted session message row, surfaced for observability export.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tool_calls_json: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub thread_id: Option<String>,
    pub turn_index: Option<i64>,
}

impl SessionStore {
    /// Upsert a thread record. Call when a Thread is first created for a
    /// session.
    pub async fn save_thread(
        &self,
        session_id: &str,
        thread_id: &str,
        label: &str,
        created_at_ms: i64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO threads (id, session_id, label, created_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(id, session_id) DO UPDATE SET label = excluded.label
            "#,
        )
        .bind(thread_id)
        .bind(session_id)
        .bind(label)
        .bind(created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to save thread".to_string(),
            details: e.to_string(),
        })?;

        debug!("Thread saved: {} / {}", session_id, thread_id);
        Ok(())
    }

    /// Append a completed turn as two rows (user + assistant) tagged with
    /// thread and turn metadata.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_turn(
        &self,
        session_id: &str,
        thread_id: &str,
        turn_index: i64,
        user_msg: &str,
        assistant_msg: &str,
        state: &str,
        turn_id: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();

        // Insert user message row.
        sqlx::query(
            r#"
            INSERT INTO session_messages (session_id, role, content, created_at, thread_id, turn_index, turn_state, turn_id)
            VALUES (?, 'user', ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(session_id)
        .bind(user_msg)
        .bind(now)
        .bind(thread_id)
        .bind(turn_index)
        .bind(state)
        .bind(turn_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to insert turn user message".to_string(),
            details: e.to_string(),
        })?;

        // Insert assistant message row.
        sqlx::query(
            r#"
            INSERT INTO session_messages (session_id, role, content, created_at, thread_id, turn_index, turn_state, turn_id)
            VALUES (?, 'assistant', ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(session_id)
        .bind(assistant_msg)
        .bind(now + 1) // ensure deterministic ordering
        .bind(thread_id)
        .bind(turn_index)
        .bind(state)
        .bind(turn_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to insert turn assistant message".to_string(),
            details: e.to_string(),
        })?;

        // Keep session message_count in sync.
        sqlx::query(
            "UPDATE sessions SET message_count = message_count + 2, last_activity = ? WHERE id = ?",
        )
        .bind(now)
        .bind(session_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: format!("Failed to increment session message_count (+2) for {}", session_id),
            details: e.to_string(),
        })?;

        debug!("Turn appended: {}/{} index={}", session_id, thread_id, turn_index);
        Ok(())
    }

    /// Load a session's messages oldest-first with their turn metadata
    /// (`thread_id`, `turn_index`). Used by `syscity observe export` to
    /// reconstruct the full (untruncated) message context of a turn.
    pub async fn get_session_messages_with_turns(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredMessage>> {
        let rows = sqlx::query(
            r#"
            SELECT role, content, reasoning_content, tool_calls_json, created_at,
                   thread_id, turn_index
            FROM session_messages
            WHERE session_id = ?
            ORDER BY id
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to load session messages for observability export".to_string(),
            details: e.to_string(),
        })?;

        let mut messages = Vec::with_capacity(rows.len());
        for row in rows {
            let role: String = row.get("role");
            let content: String = row.get("content");
            let reasoning_content: Option<String> = row.get("reasoning_content");
            let tool_calls_json: Option<String> = row.get("tool_calls_json");
            let created_at_ms: i64 = row.get("created_at");
            let thread_id: Option<String> = row.get("thread_id");
            let turn_index: Option<i64> = row.get("turn_index");
            messages.push(StoredMessage {
                role,
                content,
                reasoning_content,
                tool_calls_json,
                created_at: chrono::DateTime::from_timestamp_millis(created_at_ms)
                    .unwrap_or(Utc::now()),
                thread_id,
                turn_index,
            });
        }
        Ok(messages)
    }

    /// Load all threads for a session together with their turns.
    ///
    /// Returns `Vec<(thread_id, label, created_at_ms, Vec<(turn_index,
    /// user_msg, asst_msg, state)>)>`.
    pub async fn load_threads_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String, i64, Vec<(i64, String, String, String)>)>> {
        // Load thread rows.
        let thread_rows = sqlx::query(
            "SELECT id, label, created_at FROM threads WHERE session_id = ? ORDER BY created_at",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to load threads".to_string(),
            details: e.to_string(),
        })?;

        let mut result = Vec::new();

        for trow in thread_rows {
            let tid: String = trow.get("id");
            let label: String = trow.get("label");
            let created_at: i64 = trow.get("created_at");

            // Load user-half of each turn (role='user') ordered by turn_index.
            // We join with the assistant row by matching (session_id, thread_id,
            // turn_index).
            let turn_rows = sqlx::query(
                r#"
                SELECT u.turn_index,
                       u.content      AS user_msg,
                       COALESCE(a.content, '') AS asst_msg,
                       COALESCE(u.turn_state, 'complete') AS state
                FROM session_messages u
                LEFT JOIN session_messages a
                    ON  a.session_id = u.session_id
                    AND a.thread_id  = u.thread_id
                    AND a.turn_index = u.turn_index
                    AND a.role       = 'assistant'
                WHERE u.session_id = ?
                  AND u.thread_id  = ?
                  AND u.role       = 'user'
                ORDER BY u.turn_index
                "#,
            )
            .bind(session_id)
            .bind(&tid)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to load turns".to_string(),
                details: e.to_string(),
            })?;

            let turns: Vec<(i64, String, String, String)> = turn_rows
                .into_iter()
                .map(|r| {
                    let idx: i64 = r.get("turn_index");
                    let user: String = r.get("user_msg");
                    let asst: String = r.get("asst_msg");
                    let st: String = r.get("state");
                    (idx, user, asst, st)
                })
                .collect();

            result.push((tid, label, created_at, turns));
        }

        debug!("Loaded {} threads for session {}", result.len(), session_id);
        Ok(result)
    }

    /// Hard-delete all rows for a specific turn (used by undo persistence).
    pub async fn delete_turn(
        &self,
        session_id: &str,
        thread_id: &str,
        turn_index: i64,
    ) -> Result<()> {
        let affected = sqlx::query(
            "DELETE FROM session_messages WHERE session_id = ? AND thread_id = ? AND turn_index = \
             ?",
        )
        .bind(session_id)
        .bind(thread_id)
        .bind(turn_index)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to delete turn".to_string(),
            details: e.to_string(),
        })?
        .rows_affected();

        // Adjust session message_count (deleted rows are usually 2).
        sqlx::query("UPDATE sessions SET message_count = MAX(0, message_count - ?) WHERE id = ?")
            .bind(affected as i64)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: format!("Failed to decrement session message_count for {}", session_id),
                details: e.to_string(),
            })?;

        debug!("Deleted turn {}/{}/{}: {} rows", session_id, thread_id, turn_index, affected);
        Ok(())
    }

    /// Get session statistics
    pub async fn get_stats(&self) -> Result<SessionStats> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to count sessions".to_string(),
                details: e.to_string(),
            })?;

        let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE is_active = 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to count active sessions".to_string(),
                details: e.to_string(),
            })?;

        let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_messages")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to count messages".to_string(),
                details: e.to_string(),
            })?;

        let subagent_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subagent_runs")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to count subagent runs".to_string(),
                details: e.to_string(),
            })?;

        Ok(SessionStats {
            total_sessions: total,
            active_sessions: active,
            total_messages: messages,
            total_subagent_runs: subagent_runs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session_store::{AppendMessageParams, SessionMetadata};

    async fn create_test_store() -> SessionStore {
        // Use in-memory SQLite for tests
        SessionStore::new(":memory:")
            .await
            .expect("Failed to create test store")
    }

    #[tokio::test]
    async fn test_save_and_load_thread() {
        let store = create_test_store().await;
        let meta = SessionMetadata::new("thread-test", "default", "cli", "local");
        store
            .save_session("thread-test", &meta, "{}")
            .await
            .unwrap();

        store
            .save_thread("thread-test", "t1", "main thread", 12345)
            .await
            .unwrap();

        let threads = store.load_threads_for_session("thread-test").await.unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].0, "t1");
        assert_eq!(threads[0].1, "main thread");
        assert_eq!(threads[0].2, 12345);
        assert!(threads[0].3.is_empty());
    }

    #[tokio::test]
    async fn test_append_turn_and_load() {
        let store = create_test_store().await;
        let meta = SessionMetadata::new("turn-test", "default", "cli", "local");
        store.save_session("turn-test", &meta, "{}").await.unwrap();
        store.save_thread("turn-test", "t1", "", 0).await.unwrap();

        store
            .append_turn("turn-test", "t1", 0, "user msg", "asst msg", "complete", Some("turn-abc"))
            .await
            .unwrap();

        let threads = store.load_threads_for_session("turn-test").await.unwrap();
        assert_eq!(threads[0].3.len(), 1);
        let (idx, user, asst, state) = &threads[0].3[0];
        assert_eq!(*idx, 0);
        assert_eq!(user, "user msg");
        assert_eq!(asst, "asst msg");
        assert_eq!(state, "complete");
    }

    #[tokio::test]
    async fn test_delete_turn() {
        let store = create_test_store().await;
        let meta = SessionMetadata::new("del-turn-test", "default", "cli", "local");
        store
            .save_session("del-turn-test", &meta, "{}")
            .await
            .unwrap();
        store
            .save_thread("del-turn-test", "t1", "", 0)
            .await
            .unwrap();

        store
            .append_turn("del-turn-test", "t1", 0, "u", "a", "complete", None)
            .await
            .unwrap();
        store.delete_turn("del-turn-test", "t1", 0).await.unwrap();

        let threads = store
            .load_threads_for_session("del-turn-test")
            .await
            .unwrap();
        assert!(threads[0].3.is_empty());
    }

    #[tokio::test]
    async fn test_get_stats() {
        let store = create_test_store().await;
        let stats = store.get_stats().await.unwrap();
        assert_eq!(stats.total_sessions, 0);
        assert_eq!(stats.active_sessions, 0);
        assert_eq!(stats.total_messages, 0);
        assert_eq!(stats.total_subagent_runs, 0);

        let meta = SessionMetadata::new("stats-test", "default", "cli", "local");
        store.save_session("stats-test", &meta, "{}").await.unwrap();
        store
            .append_message(&AppendMessageParams {
                session_id: "stats-test",
                role: "user",
                content: "hi",
                ..Default::default()
            })
            .await
            .unwrap();

        let stats2 = store.get_stats().await.unwrap();
        assert_eq!(stats2.total_sessions, 1);
        assert_eq!(stats2.active_sessions, 1);
        assert_eq!(stats2.total_messages, 1);
        assert_eq!(stats2.total_subagent_runs, 0);
    }
}
