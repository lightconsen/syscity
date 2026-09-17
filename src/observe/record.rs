//! Turn observability record types.
//!
//! One `TurnRecord` per agent turn, persisted as a JSON file under
//! `~/.syscity/turns/YYYY-MM-DD/<turn_id>.json`. Large free-text fields are
//! truncated to [`MAX_FIELD_BYTES`] at persistence time via
//! [`TurnRecord::finalize`].

use serde::{Deserialize, Serialize};

/// Maximum bytes kept for free-text fields (args, results, previews, errors).
pub const MAX_FIELD_BYTES: usize = 4096;

/// Current turn record schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Default minimum token retention ratio below which a compaction is flagged
/// `low_retention`. Mirrors `CompressionQualityConfig::min_retention_ratio`
/// (default `0.5`); the gateway config is not threaded into the compression
/// observation builders, so this constant is the value used there.
pub const DEFAULT_MIN_RETENTION_RATIO: f64 = 0.5;

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndState {
    Complete,
    Error,
    Aborted,
}

/// Token usage for a single LLM call or a whole turn.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ObservedUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

/// Where a failure originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSource {
    Llm,
    Tool,
    Internal,
}

/// A recorded failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedError {
    pub source: ErrorSource,
    pub message: String,
}

/// One round of the LLM call loop within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRoundRecord {
    pub round: u32,
    pub provider: String,
    pub model: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub usage: Option<ObservedUsage>,
    /// What the gateway itself estimated this round's prompt would cost in
    /// tokens, before the call.
    ///
    /// Recorded next to the provider's own count so the two can be compared:
    /// everything a budget decision rests on — pruning, compaction, the cost
    /// guard — uses the estimate, and `usage.prompt_tokens` is what it was
    /// actually worth. Without both on the record the difference is
    /// unmeasurable.
    #[serde(default)]
    pub estimated_prompt_tokens: Option<u32>,
    pub finish_reason: Option<String>,
    pub error: Option<String>,
    /// Full request messages serialized as JSON (untruncated at capture time).
    #[serde(default)]
    pub input: Option<String>,
    /// Full streamed output text for this round (untruncated at capture time).
    #[serde(default)]
    pub output: Option<String>,
}

/// One tool invocation within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedToolCall {
    pub round: u32,
    pub name: String,
    pub args: String,
    pub result: String,
    pub success: bool,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// One line of the append-only full trace (`full.json`). Internally tagged by
/// `type` so every JSONL line is self-describing and can be replayed in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FullTraceEvent {
    /// A complete LLM round, holding the untruncated request/response.
    Round {
        round: u32,
        /// The full request messages, kept as a JSON value (never truncated).
        request: Option<serde_json::Value>,
        /// The complete streamed output text (never truncated).
        response: Option<String>,
        usage: Option<ObservedUsage>,
        finish_reason: Option<String>,
        error: Option<String>,
    },
    /// A complete tool invocation, holding untruncated args/result.
    Tool {
        round: u32,
        name: String,
        args: String,
        result: String,
        success: bool,
        duration_ms: u64,
    },
}

/// Parse `raw` into a JSON value, falling back to a plain string value when it
/// is not valid JSON. Full-trace `request` fields are always produced by
/// `serde_json::to_string`, so the fallback is defensive only.
pub fn json_value_or_string(raw: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(raw)
        .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

// ── Decision-layer sampling (additive, backward compatible) ────────────────
//
// These structs close the §五 sampling blind spots in `docs/harness.md`: the
// model-route decision, context compression, planner DAG, and channel layer.
// All are optional / `Option` on `TurnRecord` so old consumers keep working.

/// A single model-route decision: which candidates were considered, what was
/// chosen, and whether a fallback occurred. Captured by the model router on
/// every complete/stream call (and the cost-aware path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRecord {
    /// Ordered candidate models considered (`"provider/model"` labels),
    /// including skipped (disabled / circuit-open) candidates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_chain: Vec<String>,
    /// The model actually invoked (`"provider/model"`).
    pub chosen: String,
    /// Human-readable reason for the choice (primary, fallback after N failed,
    /// cost-aware budget steering, capability re-resolution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Whether any candidate was skipped or failed before the chosen one.
    pub fallback_occurred: bool,
}

/// A context-compression event: how many tokens were reclaimed and with which
/// strategy, plus the quantified retention quality (§三). Captured in
/// `compact_context_forced`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionObservation {
    /// Milliseconds since the start of the turn.
    pub triggered_at_ms: u64,
    /// Token count of the context before compaction.
    pub tokens_before: usize,
    /// Token count of the context after compaction.
    pub tokens_after: usize,
    /// `tokens_before - tokens_after` (saturating).
    pub freed_tokens: usize,
    /// `"llm_summary"` when an LLM wrote the summary, `"heuristic_summary"`
    /// for the middle-drop fallback.
    pub strategy: String,
    /// Fraction of tokens retained after compaction: `tokens_after / tokens_before`.
    /// `1.0` when nothing was reclaimed; `0.0` when `tokens_before == 0`.
    /// Additive field: legacy records without it deserialize to `0.0`.
    #[serde(default)]
    pub retention_ratio: f64,
    /// `Some("low_retention")` when `retention_ratio` is below the configured
    /// `min_retention_ratio` (default [`DEFAULT_MIN_RETENTION_RATIO`]), else
    /// `None`. This is the measurable signal an eval gate consumes (§三).
    /// Additive field: legacy records without it deserialize to `None`.
    #[serde(default)]
    pub quality_flag: Option<String>,
}

impl CompressionObservation {
    /// Build an observation from the raw token counts, computing
    /// [`retention_ratio`](Self::retention_ratio) and the `low_retention`
    /// quality flag against `min_retention_ratio`.
    pub fn from_counts(
        triggered_at_ms: u64,
        tokens_before: usize,
        tokens_after: usize,
        strategy: impl Into<String>,
        min_retention_ratio: f64,
    ) -> Self {
        let retention_ratio = if tokens_before == 0 {
            0.0
        } else {
            tokens_after as f64 / tokens_before as f64
        };
        let quality_flag = if retention_ratio < min_retention_ratio {
            Some("low_retention".to_string())
        } else {
            None
        };
        Self {
            triggered_at_ms,
            tokens_before,
            tokens_after,
            freed_tokens: tokens_before.saturating_sub(tokens_after),
            strategy: strategy.into(),
            retention_ratio,
            quality_flag,
        }
    }
}

/// One step of a plan snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStepSnapshot {
    /// Task ID (`PlannedTask::id`).
    pub id: String,
    /// Task description / goal.
    pub goal: String,
    /// `"pending"` at snapshot time (plan steps have not run yet).
    pub status: String,
}

/// Snapshot of a planner-produced DAG, captured when a plan is created. Plan
/// turns return early before any LLM round, so the whole DAG lives here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSnapshot {
    /// Plan ID (`TaskPlan::id`).
    pub plan_id: String,
    /// Overall plan goal.
    pub goal: String,
    /// The steps in execution order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<PlanStepSnapshot>,
}

/// Inbound channel-layer observation: whether the message was debounced
/// (buffered into a combined flush), enriched (media results attached), and
/// which agent it was routed to. Captured in the gateway dispatch layer and
/// carried into the turn via `IncomingMessage` metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelObservation {
    /// Message was buffered for a debounce window and flushed with siblings.
    pub debounced: bool,
    /// Media/enrichment was applied during inbound processing.
    pub enriched: bool,
    /// Agent the message was routed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

/// A complete per-turn observability record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    pub schema_version: u32,
    pub turn_id: String,
    pub session_id: Option<String>,
    pub conversation_id: String,
    /// Stable agent identifier that handled this turn (empty for ephemeral
    /// subagents). Backward-compatible: old records without the field default
    /// to an empty string.
    #[serde(default)]
    pub agent_id: String,
    pub thread_id: String,
    pub turn_index: u32,
    pub state: TurnEndState,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub model: String,
    pub user_message_preview: String,
    pub assistant_text_preview: String,
    pub reasoning_preview: String,
    pub queue_wait_ms: Option<u64>,
    pub cache_hit: bool,
    pub error: Option<ObservedError>,
    pub usage: ObservedUsage,
    pub llm_rounds: Vec<LlmRoundRecord>,
    pub tool_calls: Vec<ObservedToolCall>,
    /// Model-route decisions made during this turn (see [`RouteRecord`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_log: Vec<RouteRecord>,
    /// Context-compression events during this turn (see [`CompressionObservation`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compressions: Vec<CompressionObservation>,
    /// Planner DAG snapshot, when this turn produced a plan (see [`PlanSnapshot`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_snapshot: Option<PlanSnapshot>,
    /// Inbound channel-layer observation (see [`ChannelObservation`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelObservation>,
}

/// Truncate `s` to at most `MAX_FIELD_BYTES` bytes on a char boundary.
pub fn truncate_field(s: &mut String) {
    if s.len() <= MAX_FIELD_BYTES {
        return;
    }
    let mut end = MAX_FIELD_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

impl TurnRecord {
    /// Apply field-size limits before persistence.
    pub fn finalize(&mut self) {
        truncate_field(&mut self.user_message_preview);
        truncate_field(&mut self.assistant_text_preview);
        truncate_field(&mut self.reasoning_preview);
        if let Some(err) = &mut self.error {
            truncate_field(&mut err.message);
        }
        for round in &mut self.llm_rounds {
            if let Some(e) = &mut round.error {
                truncate_field(e);
            }
            // Keep the summary small: the full input/output lives in full.json.
            if let Some(i) = &mut round.input {
                truncate_field(i);
            }
            if let Some(o) = &mut round.output {
                truncate_field(o);
            }
        }
        for call in &mut self.tool_calls {
            truncate_field(&mut call.args);
            truncate_field(&mut call.result);
            if let Some(e) = &mut call.error {
                truncate_field(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_field_multibyte_char_boundary() {
        // Each '中' is 3 bytes; 4096 bytes lands mid-char and must back off.
        let mut s = "中".repeat(2000);
        truncate_field(&mut s);
        assert!(s.len() <= MAX_FIELD_BYTES);
        assert!(s.is_char_boundary(s.len()));
        assert_eq!(s.len(), 4095); // 1365 chars * 3 bytes
    }

    #[test]
    fn truncate_field_short_string_untouched() {
        let mut s = "hello".to_string();
        truncate_field(&mut s);
        assert_eq!(s, "hello");
    }

    #[test]
    fn record_serde_round_trip() {
        let rec = TurnRecord {
            schema_version: SCHEMA_VERSION,
            turn_id: "t1".into(),
            session_id: Some("s1".into()),
            conversation_id: "c1".into(),
            agent_id: "main".into(),
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
            cache_hit: false,
            error: None,
            usage: ObservedUsage::default(),
            llm_rounds: vec![],
            tool_calls: vec![],
            route_log: vec![RouteRecord {
                candidate_chain: vec!["openai/gpt-4o".into(), "anthropic/claude".into()],
                chosen: "anthropic/claude".into(),
                reason: Some("fallback after 1 failed".into()),
                fallback_occurred: true,
            }],
            compressions: vec![CompressionObservation {
                triggered_at_ms: 5,
                tokens_before: 1000,
                tokens_after: 200,
                freed_tokens: 800,
                strategy: "llm_summary".into(),
                retention_ratio: 0.2,
                quality_flag: Some("low_retention".into()),
            }],
            plan_snapshot: Some(PlanSnapshot {
                plan_id: "p1".into(),
                goal: "build".into(),
                steps: vec![PlanStepSnapshot {
                    id: "task_1".into(),
                    goal: "setup".into(),
                    status: "pending".into(),
                }],
            }),
            channel: Some(ChannelObservation {
                debounced: true,
                enriched: false,
                route: Some("main".into()),
            }),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: TurnRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.turn_id, "t1");
        assert_eq!(back.state, TurnEndState::Complete);
        assert_eq!(back.route_log.len(), 1);
        assert!(back.route_log[0].fallback_occurred);
        assert_eq!(back.compressions[0].freed_tokens, 800);
        assert_eq!(back.plan_snapshot.as_ref().unwrap().steps[0].id, "task_1");
        assert!(back.channel.as_ref().unwrap().debounced);
    }

    #[test]
    fn record_serde_round_trip_without_decision_fields() {
        // Old records without the additive decision-layer fields must still
        // deserialize (defaults applied).
        let json = r#"{
            "schema_version": 1,
            "turn_id": "old",
            "session_id": null,
            "conversation_id": "c",
            "agent_id": "main",
            "thread_id": "main",
            "turn_index": 0,
            "state": "complete",
            "started_at": "2026-08-14T10:00:00+08:00",
            "finished_at": "2026-08-14T10:00:01+08:00",
            "duration_ms": 1000,
            "ttft_ms": null,
            "model": "m",
            "user_message_preview": "u",
            "assistant_text_preview": "a",
            "reasoning_preview": "",
            "queue_wait_ms": null,
            "cache_hit": false,
            "error": null,
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            "llm_rounds": [],
            "tool_calls": []
        }"#;
        let back: TurnRecord = serde_json::from_str(json).unwrap();
        assert_eq!(back.turn_id, "old");
        assert!(back.route_log.is_empty());
        assert!(back.compressions.is_empty());
        assert!(back.plan_snapshot.is_none());
        assert!(back.channel.is_none());
    }

    // ── compression quality (§三) ─────────────────────────────────────────────

    fn assert_ratio(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "retention_ratio {} != {}", actual, expected);
    }

    #[test]
    fn compression_retention_ratio_half_tokens() {
        // Half the tokens retained -> 0.5.
        let obs = CompressionObservation::from_counts(0, 200, 100, "llm_summary", 0.5);
        assert_ratio(obs.retention_ratio, 0.5);
        assert_eq!(obs.freed_tokens, 100);
    }

    #[test]
    fn compression_retention_ratio_full_reclaim() {
        // Everything reclaimed -> 0.0.
        let obs = CompressionObservation::from_counts(0, 200, 0, "llm_summary", 0.5);
        assert_ratio(obs.retention_ratio, 0.0);
        assert_eq!(obs.freed_tokens, 200);
    }

    #[test]
    fn compression_retention_ratio_nothing_reclaimed() {
        // Nothing reclaimed -> 1.0.
        let obs = CompressionObservation::from_counts(0, 200, 200, "llm_summary", 0.5);
        assert_ratio(obs.retention_ratio, 1.0);
        assert_eq!(obs.freed_tokens, 0);
    }

    #[test]
    fn compression_retention_ratio_zero_before() {
        // No before tokens -> 0.0 (cannot measure retention).
        let obs = CompressionObservation::from_counts(0, 0, 0, "llm_summary", 0.5);
        assert_ratio(obs.retention_ratio, 0.0);
        assert_eq!(obs.freed_tokens, 0);
    }

    #[test]
    fn compression_quality_flag_below_threshold() {
        let obs = CompressionObservation::from_counts(0, 1000, 200, "llm_summary", 0.5);
        assert_ratio(obs.retention_ratio, 0.2);
        assert_eq!(obs.quality_flag.as_deref(), Some("low_retention"));
    }

    #[test]
    fn compression_quality_flag_at_threshold_is_clean() {
        // Exactly at the threshold is NOT low retention.
        let obs = CompressionObservation::from_counts(0, 1000, 500, "llm_summary", 0.5);
        assert_ratio(obs.retention_ratio, 0.5);
        assert_eq!(obs.quality_flag, None);
    }

    #[test]
    fn compression_quality_flag_above_threshold_is_clean() {
        let obs = CompressionObservation::from_counts(0, 1000, 800, "llm_summary", 0.5);
        assert_ratio(obs.retention_ratio, 0.8);
        assert_eq!(obs.quality_flag, None);
    }

    #[test]
    fn compression_quality_flag_default_threshold() {
        // DEFAULT_MIN_RETENTION_RATIO drives the flag when the caller relies on it.
        let low = CompressionObservation::from_counts(
            0,
            1000,
            300,
            "heuristic_summary",
            DEFAULT_MIN_RETENTION_RATIO,
        );
        assert_eq!(low.quality_flag.as_deref(), Some("low_retention"));
    }

    #[test]
    fn legacy_compression_observation_json_round_trips() {
        // A legacy observation without the additive quality fields deserializes
        // with defaults (0.0 / None), so old stored JSON keeps parsing.
        let json = r#"{
            "triggered_at_ms": 5,
            "tokens_before": 1000,
            "tokens_after": 200,
            "freed_tokens": 800,
            "strategy": "llm_summary"
        }"#;
        let obs: CompressionObservation = serde_json::from_str(json).unwrap();
        assert_eq!(obs.triggered_at_ms, 5);
        assert_eq!(obs.tokens_before, 1000);
        assert_eq!(obs.freed_tokens, 800);
        assert_eq!(obs.strategy, "llm_summary");
        assert_ratio(obs.retention_ratio, 0.0);
        assert_eq!(obs.quality_flag, None);
    }

    #[test]
    fn compression_observation_serde_round_trip_with_new_fields() {
        let obs = CompressionObservation::from_counts(7, 1000, 200, "llm_summary", 0.5);
        let json = serde_json::to_string(&obs).unwrap();
        let back: CompressionObservation = serde_json::from_str(&json).unwrap();
        assert_ratio(back.retention_ratio, 0.2);
        assert_eq!(back.quality_flag.as_deref(), Some("low_retention"));
    }
}
