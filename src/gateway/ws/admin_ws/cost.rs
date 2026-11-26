//! WS admin handlers: cost guard.

use std::sync::Arc;

use super::super::{WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Cost ────────────────────────────────────────────────────────────────

/// `cost.get` — the cost guard's current state.
///
/// Reports both the limits and what has been consumed against them, so it is
/// possible to see *why* a guard tripped before clearing it. `agent_id` narrows
/// the per-agent figures to one agent; without it every agent that has spent
/// anything is listed, which is what an operator debugging "who used the
/// budget" actually wants.
pub(crate) async fn handle_cost_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let guard = &state.agents.cost_guard;
    let agent_id = {
        #[derive(serde::Deserialize)]
        struct Params {
            #[serde(default)]
            agent_id: Option<String>,
        }
        let p: Params = match serde_json::from_value(
            req.params.clone().unwrap_or_else(|| serde_json::json!({})),
        ) {
            Ok(p) => p,
            Err(e) => {
                return WsResponse::err(&req.id, "INVALID_PARAMS", format!("Invalid params: {}", e))
            }
        };
        p.agent_id
    };

    let mut payload = serde_json::json!({
        "daily_spend_cents": guard.daily_spend_cents(),
        "daily_limit_cents": guard.daily_limit_cents,
        "hourly_actions": guard.hourly_action_count(),
        "hourly_action_limit": guard.hourly_action_limit,
        "exceeded": guard.is_exceeded(),
    });
    // The listed agents are the union of the ones carrying a configured cap and
    // the ones that have actually spent: an agent with no cap is precisely the
    // one an operator hunting a runaway loop needs to see, and an agent with a
    // cap and no spend is the one they need to see first.
    let (ids, limits): (Vec<String>, Vec<(u64, u64)>) = {
        let config = state.config.read().await;
        match &agent_id {
            Some(id) => (vec![id.clone()], vec![config.agent_cost_limits(id)]),
            None => {
                let mut ids: Vec<String> = config
                    .per_agent_cost_limits()
                    .into_iter()
                    .map(|(id, _, _)| id)
                    .collect();
                for (id, _) in guard.agents_with_spend() {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                let limits = ids.iter().map(|id| config.agent_cost_limits(id)).collect();
                (ids, limits)
            }
        }
    };
    payload["agents"] = serde_json::Value::Array(
        ids.iter()
            .zip(limits)
            .map(|(id, (daily_limit, hourly_limit))| {
                let spend = guard.agent_spend(id);
                serde_json::json!({
                    "agent_id": id,
                    "daily_spend_cents": spend.daily_spend_cents,
                    "daily_limit_cents": daily_limit,
                    "hourly_actions": spend.hourly_actions,
                    "hourly_action_limit": hourly_limit,
                    "exceeded": guard.is_exceeded_for(id),
                })
            })
            .collect(),
    );
    WsResponse::ok(&req.id, payload)
}

/// `cost.reset` — clear a tripped guard.
///
/// The guard also clears itself when its window rolls over; this is for the
/// operator who raised the limit and wants the agent working again *now*
/// rather than waiting out the remainder of the hour.
pub(crate) async fn handle_cost_reset(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let guard = &state.agents.cost_guard;
    let was_exceeded = guard.is_exceeded();
    // Rebase the window rather than only dropping the flag: an operator asking
    // for a reset wants the agent working, not one more call before it trips
    // again on a counter that is still over the limit.
    guard.clear_and_rebase();
    tracing::info!("cost guard cleared over WS (was_exceeded={was_exceeded})");
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "status": "cleared", "was_exceeded": was_exceeded }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;
    use std::sync::atomic::Ordering;

    fn req(id: &str) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: "x".into(),
            params: None,
        }
    }

    #[tokio::test]
    async fn cost_get_reports_limits_and_usage() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state
            .agents
            .cost_guard
            .budget_exceeded
            .store(true, Ordering::Release);

        let resp = handle_cost_get(&req("r1"), &state).await;

        assert!(resp.ok);
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["exceeded"], true);
        assert!(payload["daily_limit_cents"].is_number());
        assert!(payload["hourly_action_limit"].is_number());
    }

    /// The operator's escape hatch: a tripped guard stops the agent until its
    /// window rolls over, so clearing it on demand has to be reachable.
    #[tokio::test]
    async fn cost_reset_clears_a_tripped_guard() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state
            .agents
            .cost_guard
            .budget_exceeded
            .store(true, Ordering::Release);
        assert!(state.agents.cost_guard.is_exceeded(), "tripped to start");

        let resp = handle_cost_reset(&req("r1"), &state).await;

        assert!(resp.ok);
        assert_eq!(resp.payload.expect("payload")["was_exceeded"], true);
        assert!(!state.agents.cost_guard.is_exceeded(), "reset must leave the guard clear");
    }

    /// Clearing an already-clear guard is a no-op, not an error.
    #[tokio::test]
    async fn cost_reset_on_a_clear_guard_is_a_noop() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp = handle_cost_reset(&req("r1"), &state).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.expect("payload")["was_exceeded"], false);
    }

    /// An agent that has spent shows up even with no configured cap: it is the
    /// one an operator hunting a runaway loop is looking for.
    #[tokio::test]
    async fn cost_get_lists_agents_that_have_spent() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state
            .agents
            .cost_guard
            .record_usage_for("runaway", 1_000_000, 0, "claude-3-sonnet");

        let resp = handle_cost_get(&req("r1"), &state).await;

        let payload = resp.payload.expect("payload");
        let agents = payload["agents"].as_array().expect("agents array");
        let entry = agents
            .iter()
            .find(|a| a["agent_id"] == "runaway")
            .expect("the spending agent is listed");
        assert_eq!(entry["daily_spend_cents"], 300);
        assert_eq!(entry["daily_limit_cents"], 0, "it carries no cap");
        assert_eq!(entry["exceeded"], false);
    }

    /// `agent_id` narrows the report to that one agent, cap or no cap.
    #[tokio::test]
    async fn cost_get_can_be_narrowed_to_one_agent() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let mut params = req("r1");
        params.params = Some(serde_json::json!({ "agent_id": "alice" }));

        let resp = handle_cost_get(&params, &state).await;

        let payload = resp.payload.expect("payload");
        let agents = payload["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["agent_id"], "alice");
    }
}
