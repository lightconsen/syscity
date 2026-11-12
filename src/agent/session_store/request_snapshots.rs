//! Per-request snapshots of what was actually sent to the LLM.
//!
//! Debugging side table ("why did the model behave oddly that turn?"): one
//! row per outgoing LLM request recording the resolved model id, the system
//! prompt, and the tool names/schemas offered. Deliberately kept out of the
//! main `session_messages` log — rows can be large (the system prompt alone),
//! and they are write-once debug artifacts. Retention follows the same
//! observability sweep as `llm_calls` / `turn_outcomes`
//! ([`SessionStore::delete_metrics_before`] via `observe.retention_days`).
//!
//! Hot-path cost is one INSERT per LLM request, issued fire-and-forget by the
//! agent engine ([`crate::agent::Agent::persist_request_snapshot`]).

use chrono::Utc;
use sqlx::Row;
use tracing::{debug, instrument};

use crate::error::{Result, SyscityError};
use crate::providers::ToolDefinition;

use super::SessionStore;

/// Maximum number of characters stored per tool description. Descriptions are
/// debugging context, not contract; the full text still lives in the tool
/// registry source.
const MAX_TOOL_DESCRIPTION_CHARS: usize = 500;

/// A compact snapshot of one outgoing LLM request.
#[derive(Debug, Clone)]
pub struct RequestSnapshot<'a> {
    /// Owning session id, when known.
    pub session_id: Option<&'a str>,
    /// Conversation/thread id the request belongs to.
    pub conversation_id: Option<&'a str>,
    /// Agent id that issued the request.
    pub agent_id: Option<&'a str>,
    /// Model id as sent to the provider/router for this request.
    pub model: &'a str,
    /// Full system prompt as sent.
    pub system_prompt: &'a str,
    /// Compact JSON array of `{name, description, parameters}` tool defs.
    pub tools_json: &'a str,
}

/// One stored snapshot row (newest-first via [`SessionStore::load_request_snapshots`]).
#[derive(Debug, Clone)]
pub struct RequestSnapshotRow {
    pub id: i64,
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub agent_id: Option<String>,
    pub model: String,
    pub system_prompt: String,
    pub tools_json: String,
    pub created_at_ms: i64,
}

/// Truncate to at most `max` characters (char-boundary safe).
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Build the compact tools JSON stored with each snapshot.
///
/// Keeps name, description (truncated), and the parameters schema — exactly
/// what the model was offered, without the surrounding request envelope.
pub fn compact_tools_json(tools: &[ToolDefinition]) -> String {
    let defs: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.function.name,
                "description": truncate_chars(&tool.function.description, MAX_TOOL_DESCRIPTION_CHARS),
                "parameters": tool.function.parameters,
            })
        })
        .collect();
    serde_json::Value::Array(defs).to_string()
}

impl SessionStore {
    /// Insert one request-snapshot row; returns the new row id.
    #[instrument(skip(self, snapshot))]
    pub async fn save_request_snapshot(&self, snapshot: &RequestSnapshot<'_>) -> Result<i64> {
        let now = Utc::now().timestamp_millis();
        let result = sqlx::query(
            r#"
            INSERT INTO request_snapshots
                (session_id, conversation_id, agent_id, model, system_prompt,
                 tools_json, created_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(snapshot.session_id)
        .bind(snapshot.conversation_id)
        .bind(snapshot.agent_id)
        .bind(snapshot.model)
        .bind(snapshot.system_prompt)
        .bind(snapshot.tools_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to insert request snapshot".to_string(),
            details: e.to_string(),
        })?;

        debug!(
            "Saved request snapshot {} for session {:?}",
            result.last_insert_rowid(),
            snapshot.session_id
        );
        Ok(result.last_insert_rowid())
    }

    /// Load a session's request snapshots, newest first. Debugging helper;
    /// also backs round-trip tests.
    #[instrument(skip(self))]
    pub async fn load_request_snapshots(
        &self,
        session_id: &str,
        limit: i64,
    ) -> Result<Vec<RequestSnapshotRow>> {
        let rows = sqlx::query(
            r#"
            SELECT id, session_id, conversation_id, agent_id, model, system_prompt,
                   tools_json, created_at
            FROM request_snapshots
            WHERE session_id = ?
            ORDER BY id DESC
            LIMIT ?
            "#,
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: format!("Failed to load request snapshots for {}", session_id),
            details: e.to_string(),
        })?;

        Ok(rows
            .iter()
            .map(|r| RequestSnapshotRow {
                id: r.get("id"),
                session_id: r.get("session_id"),
                conversation_id: r.get("conversation_id"),
                agent_id: r.get("agent_id"),
                model: r.get("model"),
                system_prompt: r.get("system_prompt"),
                tools_json: r.get("tools_json"),
                created_at_ms: r.get("created_at"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session_store::SessionMetadata;

    async fn create_test_store() -> SessionStore {
        SessionStore::new(":memory:")
            .await
            .expect("Failed to create test store")
    }

    fn tool_def(name: &str, description: &str, params: serde_json::Value) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::providers::FunctionDefinition {
                name: name.to_string(),
                description: description.to_string(),
                parameters: params,
            },
        }
    }

    #[test]
    fn compact_tools_json_includes_names_and_schemas() {
        let long_description = "long ".repeat(200);
        let tools = vec![
            tool_def("file_read", "Read a file", serde_json::json!({"type": "object"})),
            tool_def("grep", &long_description, serde_json::json!({"type": "object"})),
        ];
        let json = compact_tools_json(&tools);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let defs = parsed.as_array().unwrap();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0]["name"], "file_read");
        assert_eq!(defs[0]["parameters"]["type"], "object");
        // Long descriptions are truncated to the documented bound.
        let desc = defs[1]["description"].as_str().unwrap();
        assert_eq!(desc.chars().count(), MAX_TOOL_DESCRIPTION_CHARS);
    }

    #[tokio::test]
    async fn save_and_load_request_snapshot_roundtrip() {
        let store = create_test_store().await;
        store
            .save_session(
                "snap-sess",
                &SessionMetadata::new("snap-sess", "default", "cli", "u"),
                "{}",
            )
            .await
            .unwrap();

        let tools = compact_tools_json(&[tool_def(
            "time",
            "Current time",
            serde_json::json!({"type": "object", "properties": {}}),
        )]);
        let snapshot = RequestSnapshot {
            session_id: Some("snap-sess"),
            conversation_id: Some("conv-1"),
            agent_id: Some("default"),
            model: "claude-haiku",
            system_prompt: "You are syscity.",
            tools_json: &tools,
        };

        let id = store.save_request_snapshot(&snapshot).await.unwrap();
        assert!(id > 0);

        let rows = store.load_request_snapshots("snap-sess", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].model, "claude-haiku");
        assert_eq!(rows[0].system_prompt, "You are syscity.");
        assert_eq!(rows[0].agent_id.as_deref(), Some("default"));
        assert_eq!(rows[0].conversation_id.as_deref(), Some("conv-1"));
        assert!(rows[0].tools_json.contains("\"time\""));
        assert!(rows[0].created_at_ms > 0);
    }

    #[tokio::test]
    async fn load_request_snapshots_orders_newest_first_and_limits() {
        let store = create_test_store().await;
        store
            .save_session(
                "order-sess",
                &SessionMetadata::new("order-sess", "default", "cli", "u"),
                "{}",
            )
            .await
            .unwrap();

        for prompt in ["first", "second", "third"] {
            let snapshot = RequestSnapshot {
                session_id: Some("order-sess"),
                conversation_id: Some("c"),
                agent_id: Some("default"),
                model: "m",
                system_prompt: prompt,
                tools_json: "[]",
            };
            store.save_request_snapshot(&snapshot).await.unwrap();
        }

        let rows = store.load_request_snapshots("order-sess", 2).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].system_prompt, "third", "newest first");
        assert_eq!(rows[1].system_prompt, "second");
    }

    #[tokio::test]
    async fn delete_metrics_before_removes_old_snapshots() {
        let store = create_test_store().await;
        let snapshot = RequestSnapshot {
            session_id: None,
            conversation_id: Some("c"),
            agent_id: None,
            model: "m",
            system_prompt: "s",
            tools_json: "[]",
        };
        store.save_request_snapshot(&snapshot).await.unwrap();

        // Prune everything up to "now" — the row was stamped with now, so cut
        // just past it.
        let cutoff = Utc::now().timestamp_millis() + 1_000;
        let (_, _, _, snapshots) = store.delete_metrics_before(cutoff).await.unwrap();
        assert_eq!(snapshots, 1);
    }
}
