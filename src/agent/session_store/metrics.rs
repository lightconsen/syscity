//! Numeric per-turn metric rows (`llm_calls`, `tool_call_metrics`,
//! `turn_outcomes`).
//!
//! Fed from the observability collector (`observe::TurnMetricsCollector`) via
//! [`SessionStore::persist_turn_metrics`]. Only numeric facts land in SQLite —
//! large free-text fields (tool args/results, previews, reasoning) stay in the
//! JSON files under `~/.syscity/turns/`. Supports `syscity observe stats` and
//! `syscity observe prune`.

use sqlx::Row;
use tracing::{debug, instrument};

use crate::error::{Result, SyscityError};
use crate::observe::record::{TurnEndState, TurnRecord};

use super::SessionStore;

/// Parse an RFC3339 timestamp (with offset) to epoch milliseconds.
/// Falls back to `now` when the string is unparseable so one bad record never
/// blocks the whole batch.
fn to_ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis())
}

impl SessionStore {
    /// Persist the numeric metrics of a whole turn record: one `turn_outcomes`
    /// row, one `llm_calls` row per LLM round, and one `tool_call_metrics` row
    /// per tool call — all in a single transaction.
    #[instrument(skip(self, rec))]
    pub async fn persist_turn_metrics(&self, rec: &TurnRecord) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|e| SyscityError::Storage {
            context: "Failed to begin metrics transaction".to_string(),
            details: e.to_string(),
        })?;

        let started_at = to_ms(&rec.started_at);
        let state = match rec.state {
            TurnEndState::Complete => "complete",
            TurnEndState::Error => "error",
            TurnEndState::Aborted => "aborted",
        };

        // ── turn_outcomes (one row per turn) ────────────────────────────────
        sqlx::query(
            r#"
            INSERT OR REPLACE INTO turn_outcomes
                (turn_id, session_id, agent_id, model, started_at, queue_wait_ms,
                 duration_ms, ttft_ms, llm_rounds, tool_calls, cache_hit, state)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&rec.turn_id)
        .bind(&rec.session_id)
        .bind(&rec.agent_id)
        .bind(&rec.model)
        .bind(started_at)
        .bind(rec.queue_wait_ms.map(|v| v as i64))
        .bind(rec.duration_ms as i64)
        .bind(rec.ttft_ms.map(|v| v as i64))
        .bind(rec.llm_rounds.len() as i64)
        .bind(rec.tool_calls.len() as i64)
        .bind(i64::from(rec.cache_hit))
        .bind(state)
        .execute(&mut *tx)
        .await
        .map_err(|e| SyscityError::Storage {
            context: "Failed to insert turn_outcomes metric".to_string(),
            details: e.to_string(),
        })?;

        // ── llm_calls (one row per LLM round) ───────────────────────────────
        for round in &rec.llm_rounds {
            let usage = round.usage;
            sqlx::query(
                r#"
                INSERT INTO llm_calls
                    (turn_id, session_id, agent_id, round, provider, model, started_at,
                     duration_ms, ttft_ms, prompt_tokens, completion_tokens,
                     cache_read_tokens, cache_creation_tokens, finish_reason, error)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(&rec.turn_id)
            .bind(&rec.session_id)
            .bind(&rec.agent_id)
            .bind(round.round)
            .bind(&round.provider)
            .bind(&round.model)
            .bind(to_ms(&round.started_at))
            .bind(round.duration_ms as i64)
            .bind(round.ttft_ms.map(|v| v as i64))
            .bind(i64::from(usage.map(|u| u.prompt_tokens).unwrap_or(0)))
            .bind(i64::from(usage.map(|u| u.completion_tokens).unwrap_or(0)))
            .bind(i64::from(usage.map(|u| u.cache_read_tokens).unwrap_or(0)))
            .bind(i64::from(usage.map(|u| u.cache_creation_tokens).unwrap_or(0)))
            .bind(&round.finish_reason)
            .bind(&round.error)
            .execute(&mut *tx)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to insert llm_calls metric".to_string(),
                details: e.to_string(),
            })?;
        }

        // ── tool_call_metrics (one row per tool call) ───────────────────────
        for call in &rec.tool_calls {
            sqlx::query(
                r#"
                INSERT INTO tool_call_metrics
                    (turn_id, session_id, round, name, started_at, duration_ms,
                     success, error)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(&rec.turn_id)
            .bind(&rec.session_id)
            .bind(call.round)
            .bind(&call.name)
            .bind(started_at)
            .bind(call.duration_ms as i64)
            .bind(i64::from(call.success))
            .bind(&call.error)
            .execute(&mut *tx)
            .await
            .map_err(|e| SyscityError::Storage {
                context: "Failed to insert tool_call_metrics metric".to_string(),
                details: e.to_string(),
            })?;
        }

        tx.commit().await.map_err(|e| SyscityError::Storage {
            context: "Failed to commit metrics transaction".to_string(),
            details: e.to_string(),
        })?;

        debug!("Persisted metrics for turn {}", rec.turn_id);
        Ok(())
    }

    /// Delete metric rows whose `started_at` is strictly older than `cutoff_ms`
    /// (epoch ms). Returns `(llm_calls, tool_call_metrics, turn_outcomes,
    /// request_snapshots)` deleted counts.
    pub async fn delete_metrics_before(&self, cutoff_ms: i64) -> Result<(u64, u64, u64, u64)> {
        async fn del(pool: &sqlx::Pool<sqlx::Sqlite>, sql: &str, cutoff_ms: i64) -> Result<u64> {
            let res = sqlx::query(sql)
                .bind(cutoff_ms)
                .execute(pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to prune metric rows".to_string(),
                    details: e.to_string(),
                })?;
            Ok(res.rows_affected())
        }

        let llm = del(&self.pool, "DELETE FROM llm_calls WHERE started_at < ?", cutoff_ms).await?;
        let tools =
            del(&self.pool, "DELETE FROM tool_call_metrics WHERE started_at < ?", cutoff_ms)
                .await?;
        let turns =
            del(&self.pool, "DELETE FROM turn_outcomes WHERE started_at < ?", cutoff_ms).await?;
        let snapshots =
            del(&self.pool, "DELETE FROM request_snapshots WHERE created_at < ?", cutoff_ms)
                .await?;
        Ok((llm, tools, turns, snapshots))
    }

    /// Query the aggregate stats from the metric tables within an optional
    /// time window (`since_ms`..`until_ms`, exclusive bounds when present).
    ///
    /// Returns a simplified stats view: turn counts by state, total LLM calls,
    /// and total tokens. Percentiles / per-model / per-tool breakdowns are
    /// computed by callers from the raw rows via [`Self::load_metric_rows`].
    pub async fn load_metric_rows(
        &self,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
    ) -> Result<MetricRows> {
        let mut since_clause = String::from(" WHERE 1=1");
        if since_ms.is_some() {
            since_clause.push_str(" AND started_at >= ?");
        }
        if until_ms.is_some() {
            since_clause.push_str(" AND started_at < ?");
        }

        let turns = {
            let sql = format!(
                "SELECT turn_id, agent_id, state, duration_ms, ttft_ms, queue_wait_ms, \
                 llm_rounds, tool_calls, cache_hit, model FROM turn_outcomes{}",
                since_clause
            );
            let mut q = sqlx::query(&sql);
            if let Some(s) = since_ms {
                q = q.bind(s);
            }
            if let Some(u) = until_ms {
                q = q.bind(u);
            }
            let rows = q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to load turn_outcomes".to_string(),
                    details: e.to_string(),
                })?;
            rows.iter()
                .map(|r| TurnOutcomeRow {
                    turn_id: r.get("turn_id"),
                    agent_id: r.get("agent_id"),
                    state: r.get("state"),
                    duration_ms: r.get("duration_ms"),
                    ttft_ms: r.get("ttft_ms"),
                    queue_wait_ms: r.get("queue_wait_ms"),
                    llm_rounds: r.get("llm_rounds"),
                    tool_calls: r.get("tool_calls"),
                    cache_hit: r.get::<i64, _>("cache_hit") != 0,
                    model: r.get("model"),
                })
                .collect()
        };

        let llm_calls = {
            let sql = format!(
                "SELECT turn_id, provider, model, duration_ms, ttft_ms, prompt_tokens, \
                 completion_tokens, cache_read_tokens, cache_creation_tokens, finish_reason, \
                 error FROM llm_calls{}",
                since_clause
            );
            let mut q = sqlx::query(&sql);
            if let Some(s) = since_ms {
                q = q.bind(s);
            }
            if let Some(u) = until_ms {
                q = q.bind(u);
            }
            let rows = q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to load llm_calls".to_string(),
                    details: e.to_string(),
                })?;
            rows.iter()
                .map(|r| LlmCallRow {
                    turn_id: r.get("turn_id"),
                    provider: r.get("provider"),
                    model: r.get("model"),
                    duration_ms: r.get("duration_ms"),
                    ttft_ms: r.get("ttft_ms"),
                    prompt_tokens: r.get("prompt_tokens"),
                    completion_tokens: r.get("completion_tokens"),
                    cache_read_tokens: r.get("cache_read_tokens"),
                    cache_creation_tokens: r.get("cache_creation_tokens"),
                    finish_reason: r.get("finish_reason"),
                    error: r.get("error"),
                })
                .collect()
        };

        let tool_calls = {
            let sql = format!(
                "SELECT turn_id, name, duration_ms, success FROM tool_call_metrics{}",
                since_clause
            );
            let mut q = sqlx::query(&sql);
            if let Some(s) = since_ms {
                q = q.bind(s);
            }
            if let Some(u) = until_ms {
                q = q.bind(u);
            }
            let rows = q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| SyscityError::Storage {
                    context: "Failed to load tool_call_metrics".to_string(),
                    details: e.to_string(),
                })?;
            rows.iter()
                .map(|r| ToolCallMetricRow {
                    turn_id: r.get("turn_id"),
                    name: r.get("name"),
                    duration_ms: r.get("duration_ms"),
                    success: r.get::<i64, _>("success") != 0,
                })
                .collect()
        };

        Ok(MetricRows { turns, llm_calls, tool_calls })
    }
}

/// A turn outcome row from `turn_outcomes`.
#[derive(Debug, Clone)]
pub struct TurnOutcomeRow {
    pub turn_id: String,
    pub agent_id: String,
    pub state: String,
    pub duration_ms: i64,
    pub ttft_ms: Option<i64>,
    pub queue_wait_ms: Option<i64>,
    pub llm_rounds: i64,
    pub tool_calls: i64,
    pub cache_hit: bool,
    pub model: String,
}

/// An LLM call row from `llm_calls`.
#[derive(Debug, Clone)]
pub struct LlmCallRow {
    pub turn_id: String,
    pub provider: String,
    pub model: String,
    pub duration_ms: i64,
    pub ttft_ms: Option<i64>,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
}

/// A tool call row from `tool_call_metrics`.
#[derive(Debug, Clone)]
pub struct ToolCallMetricRow {
    pub turn_id: String,
    pub name: String,
    pub duration_ms: i64,
    pub success: bool,
}

/// Raw metric rows for a time window, used by `syscity observe stats`.
#[derive(Debug, Default)]
pub struct MetricRows {
    pub turns: Vec<TurnOutcomeRow>,
    pub llm_calls: Vec<LlmCallRow>,
    pub tool_calls: Vec<ToolCallMetricRow>,
}

impl crate::observe::TurnMetricsSink for SessionStore {
    fn persist_turn<'a>(
        &'a self,
        rec: &'a TurnRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.persist_turn_metrics(rec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::record::{LlmRoundRecord, ObservedToolCall, ObservedUsage};

    async fn create_test_store() -> SessionStore {
        SessionStore::new(":memory:")
            .await
            .expect("Failed to create test store")
    }

    fn sample_record() -> TurnRecord {
        TurnRecord {
            schema_version: 1,
            turn_id: "t1".into(),
            session_id: Some("s1".into()),
            conversation_id: "c1".into(),
            agent_id: "worker".into(),
            thread_id: "main".into(),
            turn_index: 0,
            state: TurnEndState::Complete,
            started_at: "2026-08-14T10:00:00+08:00".into(),
            finished_at: "2026-08-14T10:00:01+08:00".into(),
            duration_ms: 1000,
            ttft_ms: Some(120),
            model: "m".into(),
            user_message_preview: "u".into(),
            assistant_text_preview: "a".into(),
            reasoning_preview: String::new(),
            queue_wait_ms: Some(5),
            cache_hit: true,
            error: None,
            usage: ObservedUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cache_read_tokens: 40,
                cache_creation_tokens: 0,
            },
            llm_rounds: vec![LlmRoundRecord {
                round: 0,
                provider: "p".into(),
                model: "m".into(),
                started_at: "2026-08-14T10:00:00+08:00".into(),
                duration_ms: 900,
                ttft_ms: Some(120),
                usage: Some(ObservedUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cache_read_tokens: 40,
                    cache_creation_tokens: 0,
                }),
                estimated_prompt_tokens: Some(120),
                finish_reason: Some("stop".into()),
                error: None,
                input: None,
                output: None,
            }],
            tool_calls: vec![ObservedToolCall {
                round: 0,
                name: "file_read".into(),
                args: "{}".into(),
                result: "ok".into(),
                success: true,
                duration_ms: 10,
                error: None,
            }],
            route_log: vec![],
            compressions: vec![],
            plan_snapshot: None,
            channel: None,
        }
    }

    #[tokio::test]
    async fn persist_metrics_inserts_three_tables() {
        let store = create_test_store().await;
        store.persist_turn_metrics(&sample_record()).await.unwrap();

        let rows = store.load_metric_rows(None, None).await.unwrap();
        assert_eq!(rows.turns.len(), 1);
        assert_eq!(rows.turns[0].state, "complete");
        assert!(rows.turns[0].cache_hit);
        assert_eq!(rows.turns[0].agent_id, "worker");
        assert_eq!(rows.llm_calls.len(), 1);
        assert_eq!(rows.llm_calls[0].prompt_tokens, 100);
        assert_eq!(rows.llm_calls[0].cache_read_tokens, 40);
        assert_eq!(rows.llm_calls[0].turn_id, "t1");
        assert_eq!(rows.tool_calls.len(), 1);
        assert_eq!(rows.tool_calls[0].name, "file_read");
        assert_eq!(rows.tool_calls[0].turn_id, "t1");
    }

    #[tokio::test]
    async fn persist_metrics_append_semantics() {
        let store = create_test_store().await;
        let rec = sample_record();
        store.persist_turn_metrics(&rec).await.unwrap();
        // Re-persisting the same turn replaces the turn_outcome row
        // (INSERT OR REPLACE) and appends llm/tool rows (per-call granularity).
        store.persist_turn_metrics(&rec).await.unwrap();

        let rows = store.load_metric_rows(None, None).await.unwrap();
        assert_eq!(rows.turns.len(), 1);
        assert_eq!(rows.llm_calls.len(), 2);
        assert_eq!(rows.tool_calls.len(), 2);
    }

    #[tokio::test]
    async fn delete_metrics_before_removes_old_rows() {
        let store = create_test_store().await;
        store.persist_turn_metrics(&sample_record()).await.unwrap();

        // The sample started_at is 2026-08-14T02:00:00Z = 1786672800000 ms.
        let (llm, tools, turns, _snapshots) =
            store.delete_metrics_before(1786672800001).await.unwrap();
        assert_eq!(llm, 1);
        assert_eq!(tools, 1);
        assert_eq!(turns, 1);

        let rows = store.load_metric_rows(None, None).await.unwrap();
        assert!(rows.turns.is_empty());
        assert!(rows.llm_calls.is_empty());
        assert!(rows.tool_calls.is_empty());
    }

    #[test]
    fn to_ms_parses_offset_timestamps() {
        assert_eq!(to_ms("2026-08-14T10:00:00+08:00"), 1786672800000);
        // Unparseable falls back to now (non-zero).
        assert!(to_ms("garbage") > 0);
    }

    #[tokio::test]
    async fn load_metric_rows_time_window_filters() {
        let store = create_test_store().await;
        store.persist_turn_metrics(&sample_record()).await.unwrap();

        // Window entirely before the record -> empty.
        let rows = store.load_metric_rows(Some(0), Some(1000)).await.unwrap();
        assert!(rows.turns.is_empty());

        // Window covering the record -> present.
        let rows = store
            .load_metric_rows(Some(1786672800000), Some(1786672800001))
            .await
            .unwrap();
        assert_eq!(rows.turns.len(), 1);
    }
}
