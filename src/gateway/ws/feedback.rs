//! WS `feedback.vote` — per-turn Like/Dislike feedback.
//!
//! The client sends the stable `turn_id` received in the `chat.final` event.
//! An `up`/`down` vote is upserted into `turn_feedback`; a `down` vote also
//! queues a `human:dislike` pending badcase (deduped on input||response).

use super::*;
use crate::eval::{InsertPendingParams, PendingSource};
use crate::gateway::FeedbackVoteKind;

/// `feedback.vote` request params.
#[derive(Debug, Deserialize)]
pub struct FeedbackVoteParams {
    /// Stable per-turn id from the `chat.final` event.
    pub turn_id: String,
    /// "up" or "down".
    pub vote: String,
    /// Optional free-text comment.
    pub comment: Option<String>,
    /// Optional user input for the turn (used to seed a `human:dislike`
    /// pending badcase; not required to record the vote itself).
    pub input: Option<String>,
    /// Optional assistant response for the turn (paired with `input`).
    pub response: Option<String>,
}

pub(super) async fn handle_feedback_vote(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let params: FeedbackVoteParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if params.turn_id.trim().is_empty() {
        return error_invalid_request(&req.id, "turn_id must be non-empty");
    }
    let vote = match FeedbackVoteKind::from_str(params.vote.trim()) {
        Some(v) => v,
        None => {
            return error_invalid_request(
                &req.id,
                format!("vote must be \"up\" or \"down\", got \"{}\"", params.vote),
            )
        }
    };

    let Some(store) = state.infra.feedback_store.as_ref() else {
        return WsResponse::err(
            &req.id,
            "FEEDBACK_UNAVAILABLE",
            "feedback store is not initialized (SQLite storage required)",
        );
    };

    if let Err(e) = store
        .upsert_vote(&crate::gateway::UpsertVoteParams {
            turn_id: params.turn_id.clone(),
            session_id: None,
            agent_id: None,
            vote,
            comment: params.comment.clone(),
        })
        .await
    {
        warn!("feedback.vote upsert failed for turn {}: {}", params.turn_id, e);
        return error_internal(&req.id, "failed to persist feedback");
    }

    // A 👎 vote seeds a pending badcase for the human-review loop, deduped on
    // input||response. Skip silently when the client did not supply both texts.
    if vote == FeedbackVoteKind::Down {
        if let (Some(input), Some(response)) = (params.input.clone(), params.response.clone()) {
            if let Some(pending) = state.infra.pending_badcase_store.as_ref() {
                match pending
                    .insert_pending(&InsertPendingParams {
                        source: PendingSource::HumanDislike,
                        turn_id: Some(params.turn_id.clone()),
                        session_id: None,
                        agent_id: None,
                        input,
                        response,
                        risk_signals: vec![],
                    })
                    .await
                {
                    Ok(true) => {
                        debug!("Queued human:dislike pending badcase for turn {}", params.turn_id)
                    }
                    Ok(false) => debug!(
                        "human:dislike pending badcase already queued for turn {}",
                        params.turn_id
                    ),
                    Err(e) => warn!(
                        "Failed to queue human:dislike pending badcase for turn {}: {}",
                        params.turn_id, e
                    ),
                }
            }
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "recorded",
            "turn_id": params.turn_id,
            "vote": vote.as_str(),
        }),
    )
}

/// `feedback.ops` — read-only, rule-based aggregation report over recent votes
/// and pending badcases (§反馈运营, no LLM).
///
/// The client may pass an optional `since_ms` window start; it defaults to
/// 30 days ago.
pub(super) async fn handle_feedback_ops(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct OpsParams {
        since_ms: Option<i64>,
    }
    let params: OpsParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let since_ms = params
        .since_ms
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() - 30 * 24 * 3600 * 1000);

    let (Some(feedback), Some(pending)) =
        (state.infra.feedback_store.as_ref(), state.infra.pending_badcase_store.as_ref())
    else {
        return WsResponse::err(
            &req.id,
            "FEEDBACK_UNAVAILABLE",
            "feedback or pending-badcase store is not initialized (SQLite storage required)",
        );
    };

    let report =
        match crate::eval::feedback_ops::build_ops_report(feedback, pending, since_ms).await {
            Ok(r) => r,
            Err(e) => {
                return error_internal(&req.id, format!("failed to build feedback ops report: {e}"))
            }
        };

    WsResponse::ok(&req.id, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state_with_store;
    use crate::gateway::GatewayConfig;

    fn make_req(id: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".to_string(),
            id: id.to_string(),
            method: "feedback.vote".to_string(),
            params: Some(params),
        }
    }

    #[tokio::test]
    async fn up_vote_records_feedback() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let res = handle_feedback_vote(
            &make_req(
                "r1",
                serde_json::json!({ "turn_id": "t1", "vote": "up", "comment": "great" }),
            ),
            &state,
        )
        .await;
        assert!(res.ok, "unexpected error: {:?}", res.error);
        assert_eq!(res.payload.as_ref().unwrap()["vote"], "up");

        let vote = state
            .infra
            .feedback_store
            .as_ref()
            .unwrap()
            .get_vote("t1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(vote.vote, FeedbackVoteKind::Up);
        assert_eq!(vote.comment.as_deref(), Some("great"));
    }

    #[tokio::test]
    async fn invalid_vote_rejected() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let res = handle_feedback_vote(
            &make_req("r1", serde_json::json!({ "turn_id": "t1", "vote": "maybe" })),
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn empty_turn_id_rejected() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let res = handle_feedback_vote(
            &make_req("r1", serde_json::json!({ "turn_id": "  ", "vote": "up" })),
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn down_vote_seeds_pending_badcase() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let res = handle_feedback_vote(
            &make_req(
                "r1",
                serde_json::json!({
                    "turn_id": "t2",
                    "vote": "down",
                    "input": "user text",
                    "response": "assistant reply",
                }),
            ),
            &state,
        )
        .await;
        assert!(res.ok, "unexpected error: {:?}", res.error);

        let pending = state
            .infra
            .pending_badcase_store
            .as_ref()
            .unwrap()
            .list_pending(crate::eval::PendingStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].source, PendingSource::HumanDislike);
        assert_eq!(pending[0].input, "user text");
        assert_eq!(pending[0].response, "assistant reply");

        // A second identical down vote dedups.
        let res2 = handle_feedback_vote(
            &make_req(
                "r2",
                serde_json::json!({
                    "turn_id": "t2",
                    "vote": "down",
                    "input": "user text",
                    "response": "assistant reply",
                }),
            ),
            &state,
        )
        .await;
        assert!(res2.ok);
        let pending2 = state
            .infra
            .pending_badcase_store
            .as_ref()
            .unwrap()
            .list_pending(crate::eval::PendingStatus::Pending, 10)
            .await
            .unwrap();
        assert_eq!(pending2.len(), 1, "down vote should dedup on input||response");
    }

    #[tokio::test]
    async fn vote_without_store_errors() {
        // make_test_state leaves stores as None.
        let state =
            Arc::new(crate::gateway::state_tests::make_test_state(GatewayConfig::default()).await);
        let res = handle_feedback_vote(
            &make_req("r1", serde_json::json!({ "turn_id": "t1", "vote": "up" })),
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "FEEDBACK_UNAVAILABLE");
    }

    #[tokio::test]
    async fn feedback_ops_aggregates_with_store() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        // A 👎 vote with input+response also seeds a human:dislike pending
        // badcase, which the report matches to enrich the down-vote summary.
        let res = handle_feedback_vote(
            &make_req(
                "v1",
                serde_json::json!({
                    "turn_id": "t1",
                    "vote": "down",
                    "input": "reset my password",
                    "response": "Your password is 12345",
                }),
            ),
            &state,
        )
        .await;
        assert!(res.ok, "unexpected error: {:?}", res.error);
        let res2 = handle_feedback_vote(
            &make_req(
                "v2",
                serde_json::json!({ "turn_id": "t2", "vote": "up", "comment": "nice" }),
            ),
            &state,
        )
        .await;
        assert!(res2.ok);

        let ops = handle_feedback_ops(&make_req("o1", serde_json::json!({})), &state).await;
        assert!(ops.ok, "unexpected error: {:?}", ops.error);
        let p = ops.payload.as_ref().unwrap();

        assert_eq!(p["total_votes"], 2);
        assert_eq!(p["up"], 1);
        assert_eq!(p["down"], 1);
        assert_eq!(p["by_day"].as_array().unwrap().len(), 14);
        // Both votes are today's bucket.
        assert_eq!(p["by_day"][13]["up"], 1);
        assert_eq!(p["by_day"][13]["down"], 1);

        let down_votes = p["down_votes"].as_array().unwrap();
        assert_eq!(down_votes.len(), 1);
        assert_eq!(down_votes[0]["turn_id"], "t1");
        assert_eq!(down_votes[0]["input"], "reset my password");

        // The response contains a high-risk pattern, so a risk cluster fires.
        let clusters = p["risk_clusters"].as_array().unwrap();
        assert!(!clusters.is_empty(), "expected a risk cluster for a flagged response");
        assert_eq!(clusters[0]["count"], 1);
        assert!(
            clusters[0]["label"].as_str().unwrap().contains("password"),
            "unexpected cluster label: {:?}",
            clusters[0]["label"]
        );
    }

    #[tokio::test]
    async fn feedback_ops_respects_since_ms() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let res = handle_feedback_vote(
            &make_req("v1", serde_json::json!({ "turn_id": "t1", "vote": "up" })),
            &state,
        )
        .await;
        assert!(res.ok);

        // A window far in the future excludes the just-recorded vote.
        let future = chrono::Utc::now().timestamp_millis() + 10 * 86_400_000;
        let ops =
            handle_feedback_ops(&make_req("o1", serde_json::json!({ "since_ms": future })), &state)
                .await;
        assert!(ops.ok, "unexpected error: {:?}", ops.error);
        let p = ops.payload.as_ref().unwrap();
        assert_eq!(p["total_votes"], 0);
    }

    #[tokio::test]
    async fn feedback_ops_without_store_errors() {
        // make_test_state leaves stores as None.
        let state =
            Arc::new(crate::gateway::state_tests::make_test_state(GatewayConfig::default()).await);
        let res = handle_feedback_ops(&make_req("o1", serde_json::json!({})), &state).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "FEEDBACK_UNAVAILABLE");
    }
}
