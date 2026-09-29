//! WS `eval.*` — harness tuning control plane (§十二 可调参 / 可追溯).
//!
//! - `eval.optimizer.run` — start a one-shot scalar-optimizer pass on the
//!   default agent (CAS hot-update + decision trace).
//! - `eval.optimizer.status` — current run status / last report / pause flag.
//! - `eval.optimizer.resume` — clear the circuit-breaker pause (Phase 4 hook).
//! - `eval.optimizer.rollback` — roll a previously applied scalar back to its
//!   baseline (Phase 4 自动回滚 escape hatch).
//! - `eval.trace.list` — read decision traces.
//! - `eval.dashboard` — read-only aggregate (trace/badcase/feedback counts +
//!   optimizer status) for the eval dashboard (§八 评测看板).
//! - `eval.propose` — structural proposer: LLM/rule-based tool-description &
//!   system-prompt rewording candidates; `adopt: true` auto-applies only the
//!   `Improved` ones (each lands in `decision_traces`).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::*;
use crate::eval::{
    AdoptionReport, ComparisonVerdict, OptimizerRunParams, PendingBadcase, PendingStatus,
    ScalarOptimizer, StructuralProposer, TraceKind,
};
use crate::gateway::FeedbackVoteKind;

pub(super) async fn handle_eval_optimizer_run(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RunParams {
        max_steps: Option<u32>,
        force: Option<bool>,
    }
    let params: RunParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let runtime = state.infra.optimizer.clone();
    {
        let st = runtime.status.read().await;
        if st.running {
            return WsResponse::err(
                &req.id,
                "OPTIMIZER_BUSY",
                "an optimizer run is already in progress",
            );
        }
    }

    // Fire the run as a registered background task so it never blocks the WS
    // loop and is aborted cleanly at shutdown.
    let optimizer = Arc::new(ScalarOptimizer::new(runtime));
    let run_state = state.clone();
    let run_params = OptimizerRunParams {
        max_steps: params.max_steps,
        force: params.force.unwrap_or(false),
        shadow: None,
        verifier: None,
    };
    let handle = tokio::spawn(async move {
        optimizer.run(run_state, run_params).await;
    });
    state
        .task_registry
        .insert_join("eval.optimizer.run", handle)
        .await;

    WsResponse::ok(&req.id, serde_json::json!({ "status": "started" }))
}

pub(super) async fn handle_eval_optimizer_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let runtime = state.infra.optimizer.clone();
    let st = runtime.status.read().await;
    let paused = runtime.paused.load(Ordering::SeqCst);
    let breaker = runtime.breaker.snapshot().await;
    let cooldown = {
        let cfg = state.config.read().await;
        std::time::Duration::from_secs(cfg.eval.optimizer.guardrails.cooldown_secs)
    };
    let breaker_open = runtime.breaker.is_open(cooldown).await;
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "running": st.running,
            "paused": paused,
            "breaker": {
                "failures": breaker.failures,
                "tripped": breaker.tripped,
                "open": breaker_open,
            },
            "last_run_at": st.last_run_at,
            "last_report": st.last_report,
            "last_error": st.last_error,
        }),
    )
}

pub(super) async fn handle_eval_optimizer_resume(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    state.infra.optimizer.paused.store(false, Ordering::SeqCst);
    state.infra.optimizer.breaker.reset().await;
    WsResponse::ok(&req.id, serde_json::json!({ "status": "resumed", "paused": false }))
}

pub(super) async fn handle_eval_optimizer_rollback(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RollbackParams {
        subject: String,
        reason: Option<String>,
    }
    let params: RollbackParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if params.subject.trim().is_empty() {
        return error_invalid_request(&req.id, "subject must not be empty");
    }

    let optimizer = ScalarOptimizer::new(state.infra.optimizer.clone());
    match optimizer
        .rollback(state.clone(), &params.subject, params.reason.as_deref().unwrap_or("manual"))
        .await
    {
        Ok(report) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "status": "rolled_back",
                "subject": report.subject,
                "from": report.from,
                "to": report.to,
                "new_revision": report.new_revision,
                "reason": report.reason,
            }),
        ),
        Err(e) => error_internal(&req.id, format!("rollback failed: {e}")),
    }
}

pub(super) async fn handle_eval_trace_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ListParams {
        kind: Option<String>,
        limit: Option<u32>,
    }
    let params: ListParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let Some(store) = state.infra.decision_trace_store.as_ref() else {
        return WsResponse::err(
            &req.id,
            "TRACE_UNAVAILABLE",
            "decision trace store is not initialized (SQLite storage required)",
        );
    };

    let kind = params.kind.as_deref().and_then(TraceKind::from_str);
    if params.kind.is_some() && kind.is_none() {
        return error_invalid_request(
            &req.id,
            "unknown trace kind; expected optimizer_apply | optimizer_reject | rollback | gate_pass | gate_fail",
        );
    }
    let limit = params.limit.unwrap_or(50).min(500);

    let traces = match store.list(kind, limit).await {
        Ok(t) => t,
        Err(e) => return error_internal(&req.id, format!("failed to list traces: {}", e)),
    };
    let items: Vec<serde_json::Value> = traces
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "kind": t.kind.as_str(),
                "subject": t.subject,
                "payload": t.payload,
                "evidence": t.evidence,
                "status": t.status.as_str(),
                "decided_at": t.decided_at,
                "applied_at": t.applied_at,
            })
        })
        .collect();

    WsResponse::ok(&req.id, serde_json::json!({ "traces": items, "count": items.len() }))
}

/// How many rows to pull per store when aggregating dashboard counts. The
/// stores only expose `list`-style queries (no count endpoints), so counts are
/// aggregated client-side over a bounded, newest-first window.
const AGG_LIMIT: u32 = 50_000;
/// Number of most-recent decision traces to embed in the dashboard payload.
const RECENT_TRACE_LIMIT: usize = 20;

/// Number of daily buckets in the eval-dashboard trend series (§八 评测看板).
const TREND_DAYS: usize = 14;
/// Milliseconds per day, for daily bucketing.
const DAY_MS: i64 = 86_400_000;

/// Map a unix-ms timestamp to its daily bucket index within the 14-day trend
/// window (0 = oldest day, `TREND_DAYS-1` = today). Rows in the future or
/// older than the window return `None` (skipped).
fn trend_bucket(ms: i64, now_ms: i64) -> Option<usize> {
    let ago_days = (now_ms - ms) / DAY_MS;
    if ago_days < 0 || ago_days >= TREND_DAYS as i64 {
        return None;
    }
    Some(TREND_DAYS - 1 - ago_days as usize)
}
/// Feedback aggregation window: 30 days in milliseconds.
const FEEDBACK_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Zero-filled count map keyed by the given string labels. Ensures every known
/// label is present in the JSON payload even when its count is zero.
fn zero_counts(keys: &[&str]) -> HashMap<String, usize> {
    keys.iter().map(|k| (k.to_string(), 0)).collect()
}

/// `eval.dashboard` — read-only aggregate for the eval dashboard (§八 评测看板).
///
/// Aggregates decision-trace counts by kind/status (+ the 20 most recent
/// traces), pending-badcase counts by source/status, 30-day Like/Dislike vote
/// tallies, and the optimizer's live status. Missing stores degrade to zeros
/// rather than erroring (unlike `eval.trace.list`, which is 404-style when the
/// trace store is absent).
pub(super) async fn handle_eval_dashboard(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    // ── decision traces: per-kind + per-status counts and the recent tail ────
    let mut traces_by_kind = zero_counts(&[
        "optimizer_apply",
        "optimizer_reject",
        "rollback",
        "gate_pass",
        "gate_fail",
    ]);
    let mut traces_by_status = zero_counts(&["pending", "applied", "rejected"]);
    let mut trace_total: usize = 0;
    let mut recent_traces: Vec<serde_json::Value> = Vec::new();
    let mut all_traces: Vec<_> = Vec::new();

    if let Some(store) = state.infra.decision_trace_store.as_ref() {
        all_traces = match store.list(None, AGG_LIMIT).await {
            Ok(t) => t,
            Err(e) => {
                return error_internal(&req.id, format!("failed to load decision traces: {e}"));
            }
        };
        trace_total = all_traces.len();
        for t in &all_traces {
            *traces_by_kind
                .entry(t.kind.as_str().to_string())
                .or_insert(0) += 1;
            *traces_by_status
                .entry(t.status.as_str().to_string())
                .or_insert(0) += 1;
        }
        recent_traces = all_traces
            .iter()
            .take(RECENT_TRACE_LIMIT)
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "kind": t.kind.as_str(),
                    "subject": t.subject,
                    "status": t.status.as_str(),
                    "decided_at": t.decided_at,
                })
            })
            .collect();
    }

    // ── pending badcases: per-source + per-status counts ─────────────────────
    let mut badcases_by_source = zero_counts(&["online:risk", "human:dislike"]);
    let mut badcases_by_status = zero_counts(&["pending", "confirmed", "converted", "dismissed"]);
    let mut badcase_total: usize = 0;
    let mut all_badcases: Vec<PendingBadcase> = Vec::new();

    if let Some(store) = state.infra.pending_badcase_store.as_ref() {
        for status in [
            PendingStatus::Pending,
            PendingStatus::Confirmed,
            PendingStatus::Converted,
            PendingStatus::Dismissed,
        ] {
            match store.list_pending(status, AGG_LIMIT).await {
                Ok(rows) => all_badcases.extend(rows),
                Err(e) => {
                    return error_internal(
                        &req.id,
                        format!("failed to load pending badcases: {e}"),
                    );
                }
            }
        }
        badcase_total = all_badcases.len();
        for b in &all_badcases {
            *badcases_by_source
                .entry(b.source.as_str().to_string())
                .or_insert(0) += 1;
            *badcases_by_status
                .entry(b.status.as_str().to_string())
                .or_insert(0) += 1;
        }
    }

    // ── feedback: 30-day Like/Dislike tallies + 14-day daily series (§八) ───
    let since_ms = chrono::Utc::now().timestamp_millis() - FEEDBACK_WINDOW_MS;
    let mut feedback_up_votes: Vec<_> = Vec::new();
    let mut feedback_down_votes: Vec<_> = Vec::new();
    if let Some(store) = state.infra.feedback_store.as_ref() {
        feedback_up_votes = match store
            .list_votes_by(FeedbackVoteKind::Up, since_ms, AGG_LIMIT)
            .await
        {
            Ok(v) => v,
            Err(e) => return error_internal(&req.id, format!("failed to load feedback: {e}")),
        };
        feedback_down_votes = match store
            .list_votes_by(FeedbackVoteKind::Down, since_ms, AGG_LIMIT)
            .await
        {
            Ok(v) => v,
            Err(e) => return error_internal(&req.id, format!("failed to load feedback: {e}")),
        };
    }
    let feedback_up = feedback_up_votes.len();
    let feedback_down = feedback_down_votes.len();

    // ── trends: 14-day daily series of the core signals (§八 评测看板) ─────
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut trend_up = [0usize; TREND_DAYS];
    let mut trend_down = [0usize; TREND_DAYS];
    let mut trend_badcases = [0usize; TREND_DAYS];
    let mut trend_traces = [0usize; TREND_DAYS];
    for v in &feedback_up_votes {
        if let Some(i) = trend_bucket(v.created_at, now_ms) {
            trend_up[i] += 1;
        }
    }
    for v in &feedback_down_votes {
        if let Some(i) = trend_bucket(v.created_at, now_ms) {
            trend_down[i] += 1;
        }
    }
    for b in &all_badcases {
        if let Some(i) = trend_bucket(b.created_at, now_ms) {
            trend_badcases[i] += 1;
        }
    }
    for t in &all_traces {
        if let Some(i) = trend_bucket(t.decided_at, now_ms) {
            trend_traces[i] += 1;
        }
    }
    let trends: Vec<serde_json::Value> = (0..TREND_DAYS)
        .map(|i| {
            let day_ms = now_ms - (TREND_DAYS - 1 - i) as i64 * DAY_MS;
            let label = chrono::DateTime::<chrono::Utc>::from_timestamp(day_ms / 1000, 0)
                .map(|dt| dt.format("%m-%d").to_string())
                .unwrap_or_default();
            serde_json::json!({
                "day": label,
                "up": trend_up[i],
                "down": trend_down[i],
                "badcases": trend_badcases[i],
                "traces": trend_traces[i],
            })
        })
        .collect();

    // ── optimizer: live run status ───────────────────────────────────────────
    let runtime = state.infra.optimizer.clone();
    let st = runtime.status.read().await;
    let paused = runtime.paused.load(Ordering::SeqCst);
    let breaker = runtime.breaker.snapshot().await;
    let cooldown = {
        let cfg = state.config.read().await;
        std::time::Duration::from_secs(cfg.eval.optimizer.guardrails.cooldown_secs)
    };
    let breaker_open = runtime.breaker.is_open(cooldown).await;

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "traces": {
                "total": trace_total,
                "by_kind": serde_json::json!(traces_by_kind),
                "by_status": serde_json::json!(traces_by_status),
                "recent": recent_traces,
            },
            "badcases": {
                "total": badcase_total,
                "by_source": serde_json::json!(badcases_by_source),
                "by_status": serde_json::json!(badcases_by_status),
            },
            "feedback": {
                "since_ms": since_ms,
                "up": feedback_up,
                "down": feedback_down,
                "total": feedback_up + feedback_down,
            },
            "trends": trends,
            "optimizer": {
                "running": st.running,
                "paused": paused,
                "breaker": {
                    "failures": breaker.failures,
                    "tripped": breaker.tripped,
                    "open": breaker_open,
                },
                "last_run_at": st.last_run_at,
                "last_report": st.last_report,
                "last_error": st.last_error,
            },
        }),
    )
}

/// `eval.propose` — produce structural rewording candidates (§十一 结构化改版提议).
///
/// The default LLM provider (or a deterministic rule-based fallback) rewrites
/// LLM-facing text — tool descriptions and the system prompt — grounded in
/// pending badcases + decision traces. Every candidate is fenced (security
/// paths are locked) and judged; with `adopt: true` only `Improved` candidates
/// are applied, each recorded as a decision trace.
pub(super) async fn handle_eval_propose(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ProposeParams {
        max_candidates: Option<usize>,
        /// When `true`, auto-adopt every candidate the harness judges `Improved`.
        adopt: Option<bool>,
    }
    let params: ProposeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let max_candidates = params.max_candidates.unwrap_or(4);
    let auto_adopt = params.adopt.unwrap_or(false);

    let provider = state
        .infra
        .model_router
        .create_default_provider()
        .await
        .ok();
    let proposer = StructuralProposer::new(provider, max_candidates);
    let candidates = match proposer.propose(state).await {
        Ok(c) => c,
        Err(e) => return error_internal(&req.id, format!("eval.propose failed: {e}")),
    };

    let mut reports = Vec::new();
    if auto_adopt {
        for cand in &candidates {
            if cand.verdict != Some(ComparisonVerdict::Improved) {
                continue;
            }
            match proposer.adopt(state, cand).await {
                Ok(report) => reports.push(report),
                Err(e) => reports.push(AdoptionReport {
                    candidate_id: cand.id.clone(),
                    adopted: false,
                    reason: format!("error: {e}"),
                    new_revision: None,
                }),
            }
        }
    }

    let items: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "object": c.object.as_str(),
                "path": c.path,
                "current": c.current,
                "proposed": c.proposed,
                "reason": c.reason,
                "fenced": c.fenced,
                "evidence": c.evidence,
                "verdict": c.verdict.as_ref().map(|v| format!("{:?}", v)),
            })
        })
        .collect();

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "candidates": items,
            "count": items.len(),
            "adopted": reports,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{PendingSource, TraceStatus};
    use crate::gateway::state_tests::{make_test_state, make_test_state_with_store};
    use crate::gateway::GatewayConfig;

    fn req(id: &str, method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params: Some(params),
        }
    }

    // Minimal stub tool referenced by seeded badcases.
    struct StubFileWriteTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for StubFileWriteTool {
        fn name(&self) -> &str {
            "file_write"
        }
        fn description(&self) -> &str {
            "write a file to disk"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _context: &crate::tools::ToolContext,
        ) -> crate::Result<crate::tools::ToolExecutionResult> {
            Ok(crate::tools::ToolExecutionResult::success("ok"))
        }
    }

    fn enabled_config() -> GatewayConfig {
        let mut cfg = GatewayConfig::default();
        cfg.eval.optimizer.enabled = true;
        cfg.eval.optimizer.max_steps = 1;
        cfg
    }

    #[tokio::test]
    async fn run_spawns_and_status_reports() {
        let state = Arc::new(make_test_state_with_store(enabled_config()).await);
        let res = handle_eval_optimizer_run(
            &req("r1", "eval.optimizer.run", serde_json::json!({})),
            &state,
        )
        .await;
        assert!(res.ok, "unexpected error: {:?}", res.error);
        assert_eq!(res.payload.as_ref().unwrap()["status"], "started");

        // The background run applies a candidate; poll status until idle.
        for _ in 0..50 {
            let st = handle_eval_optimizer_status(
                &req("s", "eval.optimizer.status", serde_json::json!({})),
                &state,
            )
            .await;
            let p = st.payload.as_ref().unwrap();
            if p["running"] == false && p["last_report"].is_object() {
                let report = &p["last_report"];
                assert_eq!(report["reason"], "completed");
                assert_eq!(report["applied"].as_array().unwrap().len(), 1);
                // A decision trace was written.
                let traces = state
                    .infra
                    .decision_trace_store
                    .as_ref()
                    .unwrap()
                    .list(None, 10)
                    .await
                    .unwrap();
                assert_eq!(traces.len(), 1);
                assert_eq!(traces[0].kind, TraceKind::OptimizerApply);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("optimizer run did not finish in time");
    }

    #[tokio::test]
    async fn trace_list_roundtrips() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        // Seed a trace directly.
        let store = state.infra.decision_trace_store.as_ref().unwrap();
        store
            .record(&crate::eval::RecordTraceParams {
                kind: TraceKind::Rollback,
                subject: "default_agent.temperature".to_string(),
                payload: serde_json::json!({ "from": 0.8, "to": 0.7 }),
                evidence: serde_json::json!({ "reason": "regression" }),
                status: crate::eval::TraceStatus::Applied,
            })
            .await
            .unwrap();

        let res = handle_eval_trace_list(
            &req("r1", "eval.trace.list", serde_json::json!({ "limit": 10 })),
            &state,
        )
        .await;
        assert!(res.ok);
        let p = res.payload.as_ref().unwrap();
        assert_eq!(p["count"], 1);
        assert_eq!(p["traces"][0]["kind"], "rollback");
        assert_eq!(p["traces"][0]["subject"], "default_agent.temperature");
    }

    #[tokio::test]
    async fn trace_list_rejects_bad_kind() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let res = handle_eval_trace_list(
            &req("r1", "eval.trace.list", serde_json::json!({ "kind": "bogus" })),
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn trace_list_without_store_errors() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res =
            handle_eval_trace_list(&req("r1", "eval.trace.list", serde_json::json!({})), &state)
                .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "TRACE_UNAVAILABLE");
    }

    #[tokio::test]
    async fn dashboard_aggregates_with_store() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);

        // Seed one decision trace, one pending badcase, and one Like vote.
        let trace_store = state.infra.decision_trace_store.as_ref().unwrap();
        trace_store
            .record(&crate::eval::RecordTraceParams {
                kind: TraceKind::OptimizerApply,
                subject: "default_agent.temperature".to_string(),
                payload: serde_json::json!({ "from": 0.8, "to": 0.7 }),
                evidence: serde_json::json!({ "score": 0.9 }),
                status: TraceStatus::Applied,
            })
            .await
            .unwrap();

        let badcase_store = state.infra.pending_badcase_store.as_ref().unwrap();
        badcase_store
            .insert_pending(&crate::eval::InsertPendingParams {
                source: PendingSource::OnlineRisk,
                turn_id: Some("t1".into()),
                session_id: None,
                agent_id: None,
                input: "input".to_string(),
                response: "response".to_string(),
                risk_signals: vec!["unhelpful".to_string()],
            })
            .await
            .unwrap();

        state
            .infra
            .feedback_store
            .as_ref()
            .unwrap()
            .upsert_vote(&crate::gateway::UpsertVoteParams {
                turn_id: "t1".to_string(),
                session_id: None,
                agent_id: None,
                vote: FeedbackVoteKind::Up,
                comment: None,
            })
            .await
            .unwrap();

        let res =
            handle_eval_dashboard(&req("d", "eval.dashboard", serde_json::json!({})), &state).await;
        assert!(res.ok, "unexpected error: {:?}", res.error);
        let p = res.payload.as_ref().unwrap();

        assert_eq!(p["traces"]["total"], 1);
        assert_eq!(p["traces"]["by_kind"]["optimizer_apply"], 1);
        assert_eq!(p["traces"]["by_status"]["applied"], 1);
        assert_eq!(p["traces"]["recent"][0]["kind"], "optimizer_apply");
        assert_eq!(p["traces"]["recent"][0]["subject"], "default_agent.temperature");

        assert_eq!(p["badcases"]["total"], 1);
        assert_eq!(p["badcases"]["by_source"]["online:risk"], 1);
        assert_eq!(p["badcases"]["by_status"]["pending"], 1);

        assert_eq!(p["feedback"]["up"], 1);
        assert_eq!(p["feedback"]["down"], 0);
        assert_eq!(p["feedback"]["total"], 1);

        // ── trends: 14-day series; the seeded rows land in today's bucket ──
        let trends = p["trends"].as_array().unwrap();
        assert_eq!(trends.len(), 14);
        assert_eq!(trends[13]["up"], 1);
        assert_eq!(trends[13]["down"], 0);
        assert_eq!(trends[13]["badcases"], 1);
        assert_eq!(trends[13]["traces"], 1);

        assert_eq!(p["optimizer"]["running"], false);
        assert_eq!(p["optimizer"]["paused"], false);
        assert_eq!(p["optimizer"]["breaker"]["failures"], 0);
    }

    #[tokio::test]
    async fn dashboard_degrades_when_stores_missing() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res =
            handle_eval_dashboard(&req("d", "eval.dashboard", serde_json::json!({})), &state).await;
        assert!(res.ok, "dashboard must degrade to zeros, got: {:?}", res.error);
        let p = res.payload.as_ref().unwrap();
        assert_eq!(p["traces"]["total"], 0);
        assert_eq!(p["traces"]["recent"].as_array().unwrap().len(), 0);
        assert_eq!(p["traces"]["by_kind"]["optimizer_apply"], 0);
        assert_eq!(p["badcases"]["total"], 0);
        assert_eq!(p["badcases"]["by_source"]["online:risk"], 0);
        assert_eq!(p["feedback"]["total"], 0);
        assert_eq!(p["trends"].as_array().unwrap().len(), 14);
        assert_eq!(p["optimizer"]["running"], false);
    }

    #[test]
    fn trend_bucket_maps_days_within_window() {
        let now_ms = 1_800_000_000_000i64;
        let day = 86_400_000i64;
        // today → last slot, yesterday → 12, 13 days ago → first slot
        assert_eq!(trend_bucket(now_ms, now_ms), Some(13));
        assert_eq!(trend_bucket(now_ms - day, now_ms), Some(12));
        assert_eq!(trend_bucket(now_ms - 13 * day, now_ms), Some(0));
        // outside the window / in the future → skipped
        assert_eq!(trend_bucket(now_ms - 14 * day, now_ms), None);
        assert_eq!(trend_bucket(now_ms + day, now_ms), None);
    }

    #[tokio::test]
    async fn propose_returns_candidates_and_auto_adopts() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        // Register a tool the seeded badcase references, so the rule-based
        // proposer emits a candidate for it.
        state
            .tools
            .registry
            .register_dynamic(std::sync::Arc::new(StubFileWriteTool));
        let store = state.infra.pending_badcase_store.as_ref().unwrap();
        let params = crate::eval::InsertPendingParams {
            source: crate::eval::PendingSource::OnlineRisk,
            turn_id: None,
            session_id: None,
            agent_id: None,
            input: "use the file_write tool to save the notes".to_string(),
            response: "ok".to_string(),
            risk_signals: vec!["unhelpful_tool_usage".to_string()],
        };
        store.insert_pending(&params).await.unwrap();

        let res = handle_eval_propose(
            &req("r1", "eval.propose", serde_json::json!({ "adopt": true })),
            &state,
        )
        .await;
        assert!(res.ok, "unexpected error: {:?}", res.error);
        let p = res.payload.as_ref().unwrap();
        assert_eq!(p["count"], 1, "expected exactly one candidate");
        assert_eq!(p["candidates"][0]["path"], "file_write");
        assert_eq!(p["candidates"][0]["verdict"], "Improved");
        let adopted = p["adopted"].as_array().unwrap();
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0]["adopted"], true);

        // The adoption was recorded as a decision trace.
        let traces = state
            .infra
            .decision_trace_store
            .as_ref()
            .unwrap()
            .list(None, 10)
            .await
            .unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].kind, TraceKind::OptimizerApply);
        assert_eq!(traces[0].subject, "struct:tool_description:file_write");
    }

    #[tokio::test]
    async fn propose_without_evidence_yields_no_candidates() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let res =
            handle_eval_propose(&req("r1", "eval.propose", serde_json::json!({})), &state).await;
        assert!(res.ok);
        let p = res.payload.as_ref().unwrap();
        assert_eq!(p["count"], 0);
    }

    #[tokio::test]
    async fn resume_clears_pause() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state.infra.optimizer.paused.store(true, Ordering::SeqCst);
        let res = handle_eval_optimizer_resume(
            &req("r1", "eval.optimizer.resume", serde_json::json!({})),
            &state,
        )
        .await;
        assert!(res.ok);
        assert_eq!(
            state.infra.optimizer.paused.load(Ordering::SeqCst),
            false,
            "pause must be cleared"
        );
    }
}
