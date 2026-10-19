//! Session and message persistence.
//!
//! CRUD for `sessions` and `session_messages` rows: save/load/find sessions,
//! append/query messages, and the active/pinned/name/delete/cleanup helpers.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::Row;
use tracing::{debug, info, instrument, warn};

use crate::error::{Result, SyscityError};

use super::{AppendMessageParams, PersistedSession, SessionMetadata, SessionStore};

impl SessionStore {
    /// Save or update a session
    #[instrument(skip(self, metadata, state_json))]
    pub async fn save_session(
        &self,
        session_id: &str,
        metadata: &SessionMetadata,
        state_json: &str,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let created_at = metadata.created_at.timestamp_millis();

        sqlx::query(
            r#"
            INSERT INTO sessions (id, agent_id, channel, channel_id, created_at, last_activity, is_active, pinned, state_json, message_count, name, bound_agent_id, transcript_id, model)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                agent_id = excluded.agent_id,
                channel = excluded.channel,
                channel_id = excluded.channel_id,
                last_activity = excluded.last_activity,
                is_active = excluded.is_active,
                pinned = excluded.pinned,
                state_json = excluded.state_json,
                name = excluded.name,
                bound_agent_id = excluded.bound_agent_id,
                transcript_id = excluded.transcript_id,
                model = excluded.model
            "#,
        )
        .bind(session_id)
        .bind(&metadata.agent_id)
        .bind(&metadata.channel)
        .bind(&metadata.channel_id)
        .bind(created_at)
        .bind(now)
        .bind(metadata.is_active)
        .bind(metadata.pinned)
        .bind(state_json)
        .bind(metadata.message_count as i64)
        .bind(&metadata.name)
        .bind(&metadata.bound_agent_id)
        .bind(&metadata.transcript_id)
        .bind(&metadata.model)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage { context: "Failed to save session".to_string(), details: e.to_string() })?;

        // Update cache
        let mut cache = self.cache.write().await;
        cache.put(session_id.to_string(), Utc::now());

        debug!("Session saved: {}", session_id);
        Ok(())
    }

    /// Ensure a session row exists, creating an empty stub if absent.
    ///
    /// Unlike [`save_session`](Self::save_session) this never overwrites an
    /// existing row, so concurrent metadata writes (agent binding, model pin)
    /// are never clobbered. Safe to call from background tasks racing with the
    /// session-create handler.
    pub async fn ensure_session_row(&self, session_id: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO sessions (id, agent_id, channel, channel_id, created_at, last_activity, is_active, state_json, message_count)
            VALUES (?, '', '', '', ?, ?, 1, '{}', 0)
            "#,
        )
        .bind(session_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            warn!("Failed to ensure session row for {}: {}", session_id, e);
            SyscityError::Storage {
                context: "Failed to ensure session row".to_string(),
                details: e.to_string(),
            }
        })?;
        Ok(())
    }

    /// Load a session by ID
    #[instrument(skip(self))]
    pub async fn load_session(&self, session_id: &str) -> Result<Option<PersistedSession>> {
        let row = sqlx::query(
            r#"
            SELECT id, agent_id, channel, channel_id, created_at, last_activity, is_active, pinned, state_json, message_count, name, bound_agent_id, transcript_id, model
            FROM sessions
            WHERE id = ?
            "#,
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage { context: "Failed to load session".to_string(), details: e.to_string() })?;

        match row {
            Some(row) => {
                let metadata = SessionMetadata {
                    session_id: row.get("id"),
                    agent_id: row.get("agent_id"),
                    channel: row.get("channel"),
                    channel_id: row.get("channel_id"),
                    created_at: DateTime::from_timestamp_millis(row.get::<i64, _>("created_at"))
                        .unwrap_or_else(Utc::now),
                    last_activity: DateTime::from_timestamp_millis(
                        row.get::<i64, _>("last_activity"),
                    )
                    .unwrap_or_else(Utc::now),
                    is_active: row.get::<i64, _>("is_active") != 0,
                    pinned: row.get::<i64, _>("pinned") != 0,
                    message_count: row.get::<i64, _>("message_count") as usize,
                    name: row.get("name"),
                    bound_agent_id: row.get("bound_agent_id"),
                    transcript_id: row.get("transcript_id"),
                    model: row.get("model"),
                };

                let session = PersistedSession {
                    id: row.get("id"),
                    metadata,
                    state_json: row.get("state_json"),
                    message_count: row.get::<i64, _>("message_count"),
                };

                // Update cache
                let mut cache = self.cache.write().await;
                cache.put(session_id.to_string(), Utc::now());

                debug!("Session loaded: {}", session_id);
                Ok(Some(session))
            }
            None => {
                debug!("Session not found: {}", session_id);
                Ok(None)
            }
        }
    }

    /// Find sessions by metadata
    #[instrument(skip(self))]
    pub async fn find_sessions(
        &self,
        agent_id: Option<&str>,
        channel: Option<&str>,
        channel_id: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<SessionMetadata>> {
        let mut query = String::from(
            "SELECT id, agent_id, channel, channel_id, created_at, last_activity, is_active, \
             pinned, message_count, name, bound_agent_id, transcript_id, model FROM sessions WHERE 1=1",
        );

        if agent_id.is_some() {
            query.push_str(" AND agent_id = ?");
        }
        if channel.is_some() {
            query.push_str(" AND channel = ?");
        }
        if channel_id.is_some() {
            query.push_str(" AND channel_id = ?");
        }
        if active_only {
            query.push_str(" AND is_active = 1");
        }

        query.push_str(" ORDER BY pinned DESC, last_activity DESC");

        let mut sql_query = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                i64,
                i64,
                i64,
                i64,
                i64,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(sqlx::AssertSqlSafe(query.as_str()));

        if let Some(agent) = agent_id {
            sql_query = sql_query.bind(agent);
        }
        if let Some(ch) = channel {
            sql_query = sql_query.bind(ch);
        }
        if let Some(ch_id) = channel_id {
            sql_query = sql_query.bind(ch_id);
        }

        let rows = sql_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to find sessions".to_string(),
                details: e.to_string(),
            })?;

        let sessions: Vec<SessionMetadata> = rows
            .into_iter()
            .map(
                |(
                    id,
                    agent_id,
                    channel,
                    channel_id,
                    created_at,
                    last_activity,
                    is_active,
                    pinned,
                    message_count,
                    name,
                    bound_agent_id,
                    transcript_id,
                    model,
                )| {
                    SessionMetadata {
                        session_id: id,
                        agent_id,
                        channel,
                        channel_id,
                        created_at: DateTime::from_timestamp_millis(created_at)
                            .unwrap_or_else(Utc::now),
                        last_activity: DateTime::from_timestamp_millis(last_activity)
                            .unwrap_or_else(Utc::now),
                        is_active: is_active != 0,
                        pinned: pinned != 0,
                        message_count: message_count as usize,
                        name,
                        bound_agent_id,
                        transcript_id,
                        model,
                    }
                },
            )
            .collect();

        debug!("Found {} sessions", sessions.len());
        Ok(sessions)
    }

    /// Append a message to session history. Returns the inserted row id.
    #[instrument(skip(self, params))]
    pub async fn append_message(&self, params: &AppendMessageParams<'_>) -> Result<i64> {
        let now = Utc::now().timestamp_millis();

        // Auto-create session row if it doesn't exist (foreign key requirement)
        self.ensure_session_row(params.session_id).await?;

        let result = sqlx::query(
            r#"
            INSERT INTO session_messages (session_id, role, content, reasoning_content, tool_calls_json, created_at, metadata, transcript_id, run_id)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(params.session_id)
        .bind(params.role)
        .bind(params.content)
        .bind(params.reasoning_content)
        .bind(params.tool_calls_json)
        .bind(now)
        .bind(params.metadata_json)
        .bind(params.transcript_id)
        .bind(params.run_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to append message".to_string(),
            details: e.to_string(),
        })?;

        // Update message count
        sqlx::query(
            "UPDATE sessions SET message_count = message_count + 1, last_activity = ? WHERE id = ?",
        )
        .bind(now)
        .bind(params.session_id)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: format!("Failed to increment session message_count for {}", params.session_id),
            details: e.to_string(),
        })?;

        Ok(result.last_insert_rowid())
    }

    /// Collect every persisted text blob that may carry an attachment
    /// reference (message bodies and inline tool-call results containing the
    /// `image_ref` marker), for the attachment-store GC sweep.
    ///
    /// The LIKE prefilter keeps this cheap on large histories; the caller
    /// extracts exact digests from the returned texts.
    pub async fn attachment_reference_texts(&self) -> Result<Vec<String>> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT content FROM session_messages WHERE content LIKE '%"image_ref"%'
            UNION ALL
            SELECT tool_calls_json FROM session_messages
            WHERE tool_calls_json LIKE '%"image_ref"%'
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to scan messages for attachment references".to_string(),
            details: e.to_string(),
        })
    }

    /// Get messages for a session, ordered newest first.
    ///
    /// Returns the most recent `limit` messages whose `created_at` is strictly
    /// less than `before`. Results are ordered newest first so callers can
    /// prepend older chunks to an existing list.
    ///
    /// Returns `(id, role, content, reasoning_content, tool_calls_json,
    /// created_at, transcript_id, run_id, turn_id)`.
    #[allow(clippy::type_complexity)]
    #[instrument(skip(self))]
    pub async fn get_messages(
        &self,
        session_id: &str,
        limit: i64,
        before: Option<DateTime<Utc>>,
    ) -> Result<
        Vec<(
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            DateTime<Utc>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
    > {
        let before_ts = before.map(|dt| dt.timestamp_millis()).unwrap_or(i64::MAX);

        let rows = sqlx::query(
            r#"
            SELECT id, role, content, reasoning_content, tool_calls_json, created_at, transcript_id, run_id, turn_id
            FROM session_messages
            WHERE session_id = ? AND created_at < ?
            ORDER BY created_at DESC
            LIMIT ?
            "#,
        )
        .bind(session_id)
        .bind(before_ts)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to get messages".to_string(),
            details: e.to_string(),
        })?;

        let mut messages: Vec<_> = rows
            .into_iter()
            .map(|row| {
                let id: i64 = row.get("id");
                let role: String = row.get("role");
                let content: String = row.get("content");
                let reasoning: Option<String> = row.get("reasoning_content");
                let tool_calls: Option<String> = row.get("tool_calls_json");
                let ts: i64 = row.get("created_at");
                let dt = DateTime::from_timestamp_millis(ts).unwrap_or_else(Utc::now);
                let transcript_id: Option<String> = row.get("transcript_id");
                let run_id: Option<String> = row.get("run_id");
                let turn_id: Option<String> = row.get("turn_id");
                (id, role, content, reasoning, tool_calls, dt, transcript_id, run_id, turn_id)
            })
            .collect();

        // Reverse to newest-first order so callers can prepend older chunks.
        messages.reverse();

        Ok(messages)
    }

    /// Set session active status
    pub async fn set_session_active(&self, session_id: &str, active: bool) -> Result<()> {
        sqlx::query("UPDATE sessions SET is_active = ?, last_activity = ? WHERE id = ?")
            .bind(if active { 1 } else { 0 })
            .bind(Utc::now().timestamp_millis())
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to update session status".to_string(),
                details: e.to_string(),
            })?;

        Ok(())
    }

    /// Set session pinned status
    pub async fn set_session_pinned(&self, session_id: &str, pinned: bool) -> Result<()> {
        sqlx::query("UPDATE sessions SET pinned = ? WHERE id = ?")
            .bind(if pinned { 1 } else { 0 })
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to update session pinned status".to_string(),
                details: e.to_string(),
            })?;

        Ok(())
    }

    /// Set or clear the session's pinned model ID (`None` clears the pin).
    ///
    /// A brand-new session has no row in `sessions` until its first message is
    /// appended (the frontend keeps new sessions client-side only), so the row
    /// is auto-created here — mirroring `append_message` — before the UPDATE.
    /// Otherwise the pin would silently match 0 rows and be lost.
    pub async fn set_session_model(&self, session_id: &str, model: Option<&str>) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO sessions (id, agent_id, channel, channel_id, created_at, last_activity, is_active, state_json, message_count)
            VALUES (?, '', '', '', ?, ?, 1, '{}', 0)
            "#,
        )
        .bind(session_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to auto-create session row for model pin".to_string(),
            details: e.to_string(),
        })?;

        sqlx::query("UPDATE sessions SET model = ? WHERE id = ?")
            .bind(model)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to update session model".to_string(),
                details: e.to_string(),
            })?;

        Ok(())
    }

    /// Set session display name
    pub async fn set_session_name(&self, session_id: &str, name: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET name = ?, last_activity = ? WHERE id = ?")
            .bind(name)
            .bind(Utc::now().timestamp_millis())
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to update session name".to_string(),
                details: e.to_string(),
            })?;

        Ok(())
    }

    /// Get session display name
    pub async fn get_session_name(&self, session_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT name FROM sessions WHERE id = ?")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to get session name".to_string(),
                details: e.to_string(),
            })?;

        Ok(row.and_then(|r| r.get::<Option<String>, _>("name")))
    }

    /// Delete a session and all its messages
    #[instrument(skip(self))]
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        // Delete messages first (SQLite FK cascade requires pragma)
        sqlx::query("DELETE FROM session_messages WHERE session_id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to delete session messages".to_string(),
                details: e.to_string(),
            })?;

        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to delete session".to_string(),
                details: e.to_string(),
            })?;

        // Cache cleanup
        let mut cache = self.cache.write().await;
        cache.pop(session_id);

        info!("Session deleted: {}", session_id);
        Ok(())
    }

    /// Cleanup old inactive sessions
    #[instrument(skip(self))]
    pub async fn cleanup_old_sessions(&self, older_than: Duration) -> Result<usize> {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(older_than).unwrap_or(chrono::Duration::days(30));

        let result = sqlx::query("DELETE FROM sessions WHERE is_active = 0 AND last_activity < ?")
            .bind(cutoff.timestamp_millis())
            .execute(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to cleanup sessions".to_string(),
                details: e.to_string(),
            })?;

        let deleted = result.rows_affected() as usize;
        info!("Cleaned up {} old sessions", deleted);
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use futures::future::join_all;

    use super::*;
    use crate::agent::session_store::{AppendMessageParams, SessionMetadata};

    async fn create_test_store() -> SessionStore {
        // Use in-memory SQLite for tests
        SessionStore::new(":memory:")
            .await
            .expect("Failed to create test store")
    }

    #[tokio::test]
    async fn test_save_and_load_session() {
        let store = create_test_store().await;

        let metadata = SessionMetadata::new("test-session", "main", "discord", "user123");

        // Save session
        store
            .save_session("test-session", &metadata, r#"{"key": "value"}"#)
            .await
            .expect("Failed to save session");

        // Load session
        let loaded = store
            .load_session("test-session")
            .await
            .expect("Failed to load session")
            .expect("Session not found");

        assert_eq!(loaded.id, "test-session");
        assert_eq!(loaded.metadata.agent_id, "main");
        assert_eq!(loaded.metadata.channel, "discord");
    }

    #[tokio::test]
    async fn test_session_model_roundtrip_set_clear() {
        let store = create_test_store().await;

        // Save a session with a pinned model.
        let mut metadata = SessionMetadata::new("m-session", "main", "web", "user1");
        metadata.model = Some("smart".to_string());
        store
            .save_session("m-session", &metadata, "{}")
            .await
            .expect("Failed to save session");

        // Round-trip: the model survives save -> load.
        let loaded = store
            .load_session("m-session")
            .await
            .expect("Failed to load session")
            .expect("Session not found");
        assert_eq!(loaded.metadata.model.as_deref(), Some("smart"));

        // find_sessions surfaces the model too.
        let found = store
            .find_sessions(None, None, None, false)
            .await
            .expect("Failed to find sessions");
        assert_eq!(found[0].model.as_deref(), Some("smart"));

        // Update the pin via set_session_model.
        store
            .set_session_model("m-session", Some("fast"))
            .await
            .expect("Failed to set session model");
        let loaded = store
            .load_session("m-session")
            .await
            .expect("Failed to load session")
            .expect("Session not found");
        assert_eq!(loaded.metadata.model.as_deref(), Some("fast"));

        // Clearing stores NULL.
        store
            .set_session_model("m-session", None)
            .await
            .expect("Failed to clear session model");
        let loaded = store
            .load_session("m-session")
            .await
            .expect("Failed to load session")
            .expect("Session not found");
        assert_eq!(loaded.metadata.model, None);
    }

    #[tokio::test]
    async fn test_set_session_model_auto_creates_row() {
        let store = create_test_store().await;

        // Session has no row yet (as with a brand-new frontend session before
        // its first message). Setting a model must auto-create the row and
        // persist the pin rather than silently matching 0 rows.
        store
            .set_session_model("fresh-session", Some("fast"))
            .await
            .expect("Failed to set model on fresh session");

        let loaded = store
            .load_session("fresh-session")
            .await
            .expect("Failed to load session")
            .expect("Session should have been auto-created");
        assert_eq!(loaded.metadata.model.as_deref(), Some("fast"));

        // Clearing a model on the auto-created row also works.
        store
            .set_session_model("fresh-session", None)
            .await
            .expect("Failed to clear model");
        let loaded = store
            .load_session("fresh-session")
            .await
            .expect("Failed to load session")
            .expect("Session not found");
        assert_eq!(loaded.metadata.model, None);
    }

    #[tokio::test]
    async fn test_ensure_session_row_does_not_clobber() {
        let store = create_test_store().await;

        // A session created via the WS handler already has rich metadata.
        let mut metadata = SessionMetadata::new("race-session", "secretary", "web", "u1");
        metadata.bound_agent_id = Some("secretary".to_string());
        metadata.model = Some("alt".to_string());
        store
            .save_session("race-session", &metadata, "{}")
            .await
            .expect("Failed to save session");

        // `SessionManager::create_session`'s background auto-persist calls
        // ensure_session_row; it must NOT overwrite the existing metadata.
        store
            .ensure_session_row("race-session")
            .await
            .expect("Failed to ensure session row");
        let loaded = store
            .load_session("race-session")
            .await
            .expect("Failed to load session")
            .expect("Session not found");
        assert_eq!(loaded.metadata.agent_id, "secretary");
        assert_eq!(loaded.metadata.model.as_deref(), Some("alt"));

        // For a brand-new session the row is created as an empty stub.
        store
            .ensure_session_row("brand-new-session")
            .await
            .expect("Failed to ensure brand-new session row");
        let loaded = store
            .load_session("brand-new-session")
            .await
            .expect("Failed to load session")
            .expect("Session should have been auto-created");
        assert_eq!(loaded.metadata.agent_id, "");
    }

    #[tokio::test]
    async fn test_find_sessions() {
        let store = create_test_store().await;

        // Create multiple sessions
        for i in 0..3 {
            let metadata = SessionMetadata::new(
                format!("session-{}", i),
                if i == 0 { "main" } else { "coder" },
                "discord",
                format!("user{}", i),
            );
            store
                .save_session(&format!("session-{}", i), &metadata, "{}")
                .await
                .expect("Failed to save session");
        }

        // Find by agent
        let main_sessions = store
            .find_sessions(Some("main"), None, None, false)
            .await
            .expect("Failed to find sessions");
        assert_eq!(main_sessions.len(), 1);
        assert_eq!(main_sessions[0].agent_id, "main");

        // Find by channel
        let discord_sessions = store
            .find_sessions(None, Some("discord"), None, false)
            .await
            .expect("Failed to find sessions");
        assert_eq!(discord_sessions.len(), 3);
    }

    #[tokio::test]
    async fn test_messages() {
        let store = create_test_store().await;

        // Create session
        let metadata = SessionMetadata::new("msg-test", "main", "cli", "local");
        store
            .save_session("msg-test", &metadata, "{}")
            .await
            .expect("Failed to save session");

        // Append messages
        store
            .append_message(&AppendMessageParams {
                session_id: "msg-test",
                role: "user",
                content: "Hello",
                ..Default::default()
            })
            .await
            .expect("Failed to append message");

        store
            .append_message(&AppendMessageParams {
                session_id: "msg-test",
                role: "assistant",
                content: "Hi there!",
                ..Default::default()
            })
            .await
            .expect("Failed to append message");

        // Get messages
        let messages = store
            .get_messages("msg-test", 10, None)
            .await
            .expect("Failed to get messages");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].1, "user"); // Newest first
        assert_eq!(messages[0].2, "Hello");
        assert_eq!(messages[1].1, "assistant");
        assert_eq!(messages[1].2, "Hi there!");
    }

    #[tokio::test]
    async fn test_set_session_active() {
        let store = create_test_store().await;
        let meta = SessionMetadata::new("active-test", "main", "cli", "local");
        store
            .save_session("active-test", &meta, "{}")
            .await
            .unwrap();

        store
            .set_session_active("active-test", false)
            .await
            .unwrap();
        let loaded = store.load_session("active-test").await.unwrap().unwrap();
        assert!(!loaded.metadata.is_active);

        store.set_session_active("active-test", true).await.unwrap();
        let loaded2 = store.load_session("active-test").await.unwrap().unwrap();
        assert!(loaded2.metadata.is_active);
    }

    #[tokio::test]
    async fn test_delete_session() {
        let store = create_test_store().await;
        let meta = SessionMetadata::new("del-test", "main", "cli", "local");
        store.save_session("del-test", &meta, "{}").await.unwrap();

        store.delete_session("del-test").await.unwrap();
        let loaded = store.load_session("del-test").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_find_sessions_active_only() {
        let store = create_test_store().await;
        let meta1 = SessionMetadata::new("active-1", "main", "cli", "local");
        store.save_session("active-1", &meta1, "{}").await.unwrap();

        let mut meta2 = SessionMetadata::new("inactive-1", "main", "cli", "local");
        meta2.is_active = false;
        store
            .save_session("inactive-1", &meta2, "{}")
            .await
            .unwrap();

        let all = store.find_sessions(None, None, None, false).await.unwrap();
        assert_eq!(all.len(), 2);

        let active = store.find_sessions(None, None, None, true).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "active-1");
    }

    #[tokio::test]
    async fn test_cleanup_old_sessions() {
        let store = create_test_store().await;
        // save_session always sets last_activity to now, so recent sessions won't be
        // cleaned up
        let mut meta = SessionMetadata::new("old", "main", "cli", "local");
        meta.is_active = false;
        store.save_session("old", &meta, "{}").await.unwrap();

        // Cleanup with 30 days should not affect a session that was just saved
        let deleted = store
            .cleanup_old_sessions(Duration::from_secs(86400 * 30))
            .await
            .unwrap();
        assert_eq!(deleted, 0);

        // Session should still exist
        let remaining = store.load_session("old").await.unwrap();
        assert!(remaining.is_some());
    }

    #[tokio::test]
    async fn test_get_messages_with_limit() {
        let store = create_test_store().await;
        let meta = SessionMetadata::new("limit-test", "main", "cli", "local");
        store.save_session("limit-test", &meta, "{}").await.unwrap();

        for i in 0..5 {
            store
                .append_message(&AppendMessageParams {
                    session_id: "limit-test",
                    role: "user",
                    content: &format!("msg{}", i),
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let msgs = store.get_messages("limit-test", 2, None).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].2, "msg3");
        assert_eq!(msgs[1].2, "msg4");
    }

    #[tokio::test]
    async fn test_persisted_session_fields() {
        let store = create_test_store().await;
        let meta = SessionMetadata::new("persist-test", "main", "cli", "local");
        store
            .save_session("persist-test", &meta, r#"{"key":"val"}"#)
            .await
            .unwrap();

        let loaded = store.load_session("persist-test").await.unwrap().unwrap();
        assert_eq!(loaded.id, "persist-test");
        assert_eq!(loaded.state_json, r#"{"key":"val"}"#);
        assert_eq!(loaded.message_count, 0);
    }

    #[tokio::test]
    async fn test_set_session_pinned_and_find_order() {
        let store = create_test_store().await;

        let mut meta1 = SessionMetadata::new("pinned-test", "main", "cli", "local");
        meta1.pinned = true;
        store
            .save_session("pinned-test", &meta1, "{}")
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let meta2 = SessionMetadata::new("recent-test", "main", "cli", "local");
        store
            .save_session("recent-test", &meta2, "{}")
            .await
            .unwrap();

        store.set_session_pinned("recent-test", true).await.unwrap();

        let all = store.find_sessions(None, None, None, false).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].pinned);
        assert!(all[1].pinned);

        store
            .set_session_pinned("recent-test", false)
            .await
            .unwrap();
        let after = store.find_sessions(None, None, None, false).await.unwrap();
        assert!(after[0].pinned);
        assert!(!after[1].pinned);
    }

    #[tokio::test]
    async fn test_concurrent_session_access_no_deadlock() {
        let store = create_test_store().await;
        let session_id = "concurrent-session";
        let meta = SessionMetadata::new(session_id, "main", "cli", "local");
        store.save_session(session_id, &meta, "{}").await.unwrap();

        let mut tasks = Vec::new();
        for i in 0..10usize {
            let store = store.clone();
            let sid = session_id.to_string();
            tasks.push(tokio::spawn(async move {
                store
                    .append_message(&AppendMessageParams {
                        session_id: &sid,
                        role: "user",
                        content: &format!("msg-{}", i),
                        ..Default::default()
                    })
                    .await
                    .expect("append should succeed");
            }));
        }
        for _ in 0..5usize {
            let store = store.clone();
            let sid = session_id.to_string();
            tasks.push(tokio::spawn(async move {
                let loaded = store.load_session(&sid).await.expect("load should succeed");
                assert!(loaded.is_some(), "session should exist");
            }));
        }
        for i in 0..5usize {
            let store = store.clone();
            let sid = session_id.to_string();
            tasks.push(tokio::spawn(async move {
                store
                    .set_session_name(&sid, &format!("name-{}", i))
                    .await
                    .expect("set_name should succeed");
            }));
        }

        join_all(tasks)
            .await
            .into_iter()
            .for_each(|r| r.expect("concurrent task should not panic"));

        let final_session = store.load_session(session_id).await.unwrap().unwrap();
        assert_eq!(final_session.message_count, 10, "all 10 concurrent appends should be recorded");
    }

    /// Simulate N concurrent sessions to measure throughput (RPS and memory).
    #[tokio::test]
    async fn test_throughput_n_concurrent_sessions() {
        let store = create_test_store().await;
        let n_sessions = 50usize;

        let start = std::time::Instant::now();

        let mut tasks = Vec::new();
        for i in 0..n_sessions {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                let sid = format!("throughput-session-{}", i);
                let meta = SessionMetadata::new(&sid, "main", "cli", "local");
                store.save_session(&sid, &meta, "{}").await.unwrap();

                // Append a few messages
                for j in 0..5usize {
                    store
                        .append_message(&AppendMessageParams {
                            session_id: &sid,
                            role: "user",
                            content: &format!("msg-{}", j),
                            ..Default::default()
                        })
                        .await
                        .unwrap();
                }

                // Load back
                let loaded = store.load_session(&sid).await.unwrap();
                assert!(loaded.is_some());
                loaded.unwrap().message_count
            }));
        }

        let results = futures::future::join_all(tasks).await;
        let elapsed = start.elapsed();

        let total_messages: usize = results.into_iter().map(|r| r.unwrap() as usize).sum();
        assert_eq!(total_messages, n_sessions * 5);

        // Rough throughput assertion: 50 sessions with 5 messages each should
        // complete in under 10 seconds even on slow CI runners.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "Throughput test too slow: {:?}",
            elapsed
        );

        // Calculate RPS (requests per second) for informational purposes
        let total_ops = n_sessions * 7; // save + 5 appends + load
        let rps = total_ops as f64 / elapsed.as_secs_f64();
        println!(
            "Throughput: {} sessions, {} ops in {:?} = {:.1} RPS",
            n_sessions, total_ops, elapsed, rps
        );
    }
}
