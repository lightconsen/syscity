//! Crash-recovery balance repair for persisted session history.
//!
//! If the process dies mid-turn, the persisted session can contain an
//! assistant message whose `tool_calls_json` holds tool calls that never
//! received a result row. An unbalanced call/result sequence confuses any
//! consumer that replays the history into an LLM: strict providers reject it,
//! and lenient ones invite the model to invent a successful outcome for a call
//! that may actually have produced side effects.
//!
//! [`SessionStore::repair_orphan_tool_calls`] detects those orphans at
//! session-load time and appends one synthetic `tool` result row per orphaned
//! call, prefixed with the [`TOOL_OUTCOME_UNKNOWN`] sentinel plus a short note
//! that side effects are possible. The repair is idempotent: synthetic rows
//! are tagged in their `metadata` JSON, and already-satisfied call ids are
//! skipped on subsequent loads, so loading twice never duplicates rows.
//!
//! Call this from every path that rehydrates model-facing history from
//! `session_messages` (currently [`crate::agent::Agent::restore_threads`]).

use serde_json::json;
use sqlx::Row;
use tracing::{debug, instrument};

use crate::error::{Result, SyscityError};
use crate::providers::ToolCall;

use super::{AppendMessageParams, SessionStore};

/// Sentinel prefixed to synthetic tool-result content inserted by
/// [`SessionStore::repair_orphan_tool_calls`]. Signals "the outcome of this
/// call is unknown", not success or failure.
pub const TOOL_OUTCOME_UNKNOWN: &str = "TOOL_OUTCOME_UNKNOWN";

/// Metadata key on a synthetic `tool` row holding the id of the tool call it
/// satisfies.
const SATISFIES_KEY: &str = "synthetic_tool_result";

/// Alternative metadata key for rows written by other persistence paths that
/// store the satisfied call id under the provider-style name.
const REAL_RESULT_KEY: &str = "tool_call_id";

/// One orphaned tool call found by the balance scan: its call id and the
/// tool name (for a readable explanation in the synthetic result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanToolCall {
    pub call_id: String,
    pub tool_name: String,
}

/// Minimal view of one persisted message row for the balance scan.
pub(crate) struct BalanceRow {
    pub role: String,
    pub tool_calls_json: Option<String>,
    pub metadata: Option<String>,
}

/// Extract the metadata value satisfying a tool-call id from a `tool` row.
fn satisfied_call_id(metadata: Option<&str>) -> Option<String> {
    let raw = metadata?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    [SATISFIES_KEY, REAL_RESULT_KEY]
        .iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()).map(String::from))
}

/// Scan ordered (oldest-first) rows for tool calls without a matching result.
///
/// A call is orphaned when all of the following hold:
/// - it appears in an assistant row's `tool_calls_json`,
/// - its embedded `result` field is empty (completed turns persist the outcome
///   inline, so those calls are healthy), and
/// - no later `tool` row's metadata claims to satisfy its id.
///
/// Returns orphans deduplicated by call id, in first-seen order.
pub(crate) fn orphan_tool_calls(rows: &[BalanceRow]) -> Vec<OrphanToolCall> {
    // Ids already satisfied by a tool-result row (synthetic or real).
    let mut satisfied: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in rows {
        if row.role == "tool" {
            if let Some(id) = satisfied_call_id(row.metadata.as_deref()) {
                satisfied.insert(id);
            }
        }
    }

    let mut orphans: Vec<OrphanToolCall> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in rows {
        if row.role != "assistant" {
            continue;
        }
        let Some(ref raw) = row.tool_calls_json else {
            continue;
        };
        let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(raw) else {
            debug!("Skipping unparseable tool_calls_json during balance scan");
            continue;
        };
        for call in calls {
            if call.result.as_deref().is_some_and(|r| !r.is_empty()) {
                continue; // Outcome was recorded inline; nothing to balance.
            }
            if satisfied.contains(&call.id) || !seen.insert(call.id.clone()) {
                continue;
            }
            orphans.push(OrphanToolCall {
                call_id: call.id,
                tool_name: call.function.name,
            });
        }
    }
    orphans
}

/// Content of the synthetic tool-result row for an orphaned call.
fn synthetic_content(tool_name: &str) -> String {
    format!(
        "{} Tool '{}' was called but the process restarted before its result \
         was recorded. The call may have produced side effects; treat its \
         outcome as unknown rather than assuming success.",
        TOOL_OUTCOME_UNKNOWN, tool_name
    )
}

impl SessionStore {
    /// Append synthetic `TOOL_OUTCOME_UNKNOWN` results for orphaned tool calls.
    ///
    /// Scans the session's persisted messages oldest-first; for every assistant
    /// tool call lacking both an inline result and a satisfying `tool` row,
    /// inserts one synthetic `role='tool'` message tagged in `metadata` with
    /// the call id. Returns the number of rows inserted (0 when the history is
    /// already balanced, making repeated loads idempotent).
    #[instrument(skip(self))]
    pub async fn repair_orphan_tool_calls(&self, session_id: &str) -> Result<usize> {
        let rows = sqlx::query(
            r#"
            SELECT role, tool_calls_json, metadata
            FROM session_messages
            WHERE session_id = ?
            ORDER BY id
            "#,
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: format!(
                "Failed to scan messages of session {} for orphaned tool calls",
                session_id
            ),
            details: e.to_string(),
        })?;

        let balance_rows: Vec<BalanceRow> = rows
            .iter()
            .map(|r| BalanceRow {
                role: r.get("role"),
                tool_calls_json: r.get("tool_calls_json"),
                metadata: r.get("metadata"),
            })
            .collect();
        let orphans = orphan_tool_calls(&balance_rows);
        if orphans.is_empty() {
            return Ok(0);
        }

        let mut repaired = 0;
        for orphan in &orphans {
            let content = synthetic_content(&orphan.tool_name);
            let metadata =
                json!({ SATISFIES_KEY: orphan.call_id, "tool": orphan.tool_name }).to_string();
            self.append_message(&AppendMessageParams {
                session_id,
                role: "tool",
                content: &content,
                metadata_json: Some(&metadata),
                ..Default::default()
            })
            .await?;
            repaired += 1;
        }

        debug!("Inserted {} synthetic tool result(s) for session {}", repaired, session_id);
        Ok(repaired)
    }

    /// Scan every persisted session for orphaned tool calls *without*
    /// repairing them. Returns one entry per session that still has unpaired
    /// calls; an empty map means every stored history is balanced.
    ///
    /// This is the read-only half of [`SessionStore::repair_orphan_tool_calls`],
    /// used by the runtime invariant registry (`syscity invariants`).
    pub async fn orphaned_tool_calls_by_session(
        &self,
    ) -> Result<Vec<(String, Vec<OrphanToolCall>)>> {
        let session_ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT session_id FROM session_messages ORDER BY session_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to list sessions for balance scan".to_string(),
            details: e.to_string(),
        })?;

        let mut unbalanced = Vec::new();
        for session_id in session_ids {
            let rows = sqlx::query(
                r#"
                SELECT role, tool_calls_json, metadata
                FROM session_messages
                WHERE session_id = ?
                ORDER BY id
                "#,
            )
            .bind(&session_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SyscityError::Storage {
                context: format!(
                    "Failed to scan messages of session {session_id} for orphaned tool calls"
                ),
                details: e.to_string(),
            })?;
            let balance_rows: Vec<BalanceRow> = rows
                .iter()
                .map(|r| BalanceRow {
                    role: r.get("role"),
                    tool_calls_json: r.get("tool_calls_json"),
                    metadata: r.get("metadata"),
                })
                .collect();
            let orphans = orphan_tool_calls(&balance_rows);
            if !orphans.is_empty() {
                unbalanced.push((session_id, orphans));
            }
        }
        Ok(unbalanced)
    }
}

/// Runtime invariant checks owned by the session store (registered via
/// `core::invariants::register_builtins`, surfaced through `syscity
/// invariants`).
pub(crate) fn invariant_checks() -> Vec<crate::core::invariants::Invariant> {
    use crate::core::invariants::{Invariant, SKIP_PREFIX};

    vec![Invariant {
        id: "agent/session_history_balanced",
        module: "agent",
        description: "every persisted session has a tool result for each tool call",
        check: || {
            Box::pin(async {
                let db_path = crate::dirs::default_memory_db();
                if !db_path.exists() {
                    return Err(format!(
                        "{SKIP_PREFIX}daemon has never run; no session store at {}",
                        db_path.display()
                    ));
                }
                let url = format!("sqlite:///{}", db_path.display());
                let store = SessionStore::new(&url).await.map_err(|e| {
                    format!("failed to open session store at {}: {e}", db_path.display())
                })?;
                let unbalanced = store
                    .orphaned_tool_calls_by_session()
                    .await
                    .map_err(|e| format!("balance scan failed: {e}"))?;
                if unbalanced.is_empty() {
                    return Ok(());
                }
                let summary: Vec<String> = unbalanced
                    .iter()
                    .map(|(sid, orphans)| format!("{} ({} orphaned)", sid, orphans.len()))
                    .collect();
                Err(format!(
                    "{} session(s) with orphaned tool calls — run the daemon once to \
                     auto-repair on load: {}",
                    unbalanced.len(),
                    summary.join(", ")
                ))
            })
        },
    }]
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

    async fn seed_session(store: &SessionStore, session_id: &str) {
        store
            .save_session(
                session_id,
                &SessionMetadata::new(session_id, "default", "cli", "local"),
                "{}",
            )
            .await
            .expect("Failed to save session");
    }

    async fn append_rows(
        store: &SessionStore,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls_json: Option<&str>,
        metadata: Option<&str>,
    ) {
        store
            .append_message(&AppendMessageParams {
                session_id,
                role,
                content,
                tool_calls_json,
                metadata_json: metadata,
                ..Default::default()
            })
            .await
            .expect("Failed to append message");
    }

    fn calls_json(entries: &[(&str, &str, Option<&str>)]) -> String {
        let calls: Vec<serde_json::Value> = entries
            .iter()
            .map(|(id, name, result)| {
                let mut v = json!({
                    "id": id,
                    "call_type": "function",
                    "function": { "name": name, "arguments": "{}" },
                });
                if let Some(r) = result {
                    v["result"] = json!(r);
                }
                v
            })
            .collect();
        serde_json::Value::Array(calls).to_string()
    }

    fn roles_with_tool_rows(messages: &[(String, String)]) -> Vec<(String, String)> {
        messages
            .iter()
            .filter(|(role, _)| role == "tool")
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn repair_inserts_synthetic_result_exactly_once() {
        let store = create_test_store().await;
        seed_session(&store, "crash-sess").await;

        // Simulate a crash mid-turn: user message, then an assistant message
        // carrying two tool calls whose results were never recorded.
        append_rows(&store, "crash-sess", "user", "run the checks", None, None).await;
        append_rows(
            &store,
            "crash-sess",
            "assistant",
            "",
            Some(&calls_json(&[
                ("call-1", "shell_exec", None),
                ("call-2", "file_write", None),
            ])),
            None,
        )
        .await;

        // First load repairs the imbalance: one synthetic row per orphan call.
        let inserted = store.repair_orphan_tool_calls("crash-sess").await.unwrap();
        assert_eq!(inserted, 2, "one synthetic result per orphaned call");

        let messages = store
            .get_messages("crash-sess", 100, None)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, role, content, ..)| (role, content))
            .rev()
            .collect::<Vec<_>>();
        let tool_rows = roles_with_tool_rows(&messages);
        assert_eq!(tool_rows.len(), 2);
        for (_, content) in &tool_rows {
            assert!(content.starts_with(TOOL_OUTCOME_UNKNOWN), "sentinel present");
            assert!(content.contains("side effects"), "side-effect warning present");
        }
        assert!(
            tool_rows.iter().any(|(_, c)| c.contains("shell_exec")),
            "tool name appears in the explanation"
        );

        // Loading again must not duplicate the synthetic rows.
        let second = store.repair_orphan_tool_calls("crash-sess").await.unwrap();
        assert_eq!(second, 0, "repair is idempotent across loads");

        let messages_after = store.get_messages("crash-sess", 100, None).await.unwrap();
        assert_eq!(messages_after.len(), 4, "user + assistant + exactly 2 synthetic rows");
    }

    #[tokio::test]
    async fn completed_turns_are_not_repaired() {
        let store = create_test_store().await;
        seed_session(&store, "healthy-sess").await;

        // Completed turn: outcomes were merged into the persisted tool calls.
        append_rows(
            &store,
            "healthy-sess",
            "assistant",
            "done",
            Some(&calls_json(&[("call-ok", "web_fetch", Some("{\"status\":200}"))])),
            None,
        )
        .await;

        let inserted = store
            .repair_orphan_tool_calls("healthy-sess")
            .await
            .unwrap();
        assert_eq!(inserted, 0, "calls with inline results need no repair");

        let messages = store.get_messages("healthy-sess", 10, None).await.unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn existing_tool_rows_satisfy_pairing() {
        let store = create_test_store().await;
        seed_session(&store, "paired-sess").await;

        append_rows(
            &store,
            "paired-sess",
            "assistant",
            "",
            Some(&calls_json(&[("c9", "grep", None)])),
            None,
        )
        .await;
        // A real tool-result row (metadata carries the satisfied call id).
        append_rows(
            &store,
            "paired-sess",
            "tool",
            "no matches",
            None,
            Some(r#"{"synthetic_tool_result":"c9","tool":"grep"}"#),
        )
        .await;

        let inserted = store.repair_orphan_tool_calls("paired-sess").await.unwrap();
        assert_eq!(inserted, 0, "already-paired calls are skipped");
    }

    #[test]
    fn orphan_scan_deduplicates_and_skips_unparseable_rows() {
        let rows = vec![
            BalanceRow {
                role: "user".to_string(),
                tool_calls_json: None,
                metadata: None,
            },
            BalanceRow {
                role: "assistant".to_string(),
                tool_calls_json: Some(calls_json(&[("a", "t1", None), ("b", "t2", Some("ok"))])),
                metadata: None,
            },
            BalanceRow {
                role: "assistant".to_string(),
                tool_calls_json: Some("not json".to_string()),
                metadata: None,
            },
            BalanceRow {
                role: "assistant".to_string(),
                tool_calls_json: Some(calls_json(&[("a", "t1", None)])),
                metadata: None,
            },
        ];
        let orphans = orphan_tool_calls(&rows);
        assert_eq!(
            orphans,
            vec![OrphanToolCall {
                call_id: "a".to_string(),
                tool_name: "t1".to_string()
            }],
            "inline results and duplicate ids are skipped; bad JSON tolerated"
        );
    }

    #[test]
    fn synthetic_content_carries_sentinel_and_warning() {
        let content = synthetic_content("shell_exec");
        assert!(content.starts_with(TOOL_OUTCOME_UNKNOWN));
        assert!(content.contains("shell_exec"));
        assert!(content.contains("side effects"));
    }
}
