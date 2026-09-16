//! WS admin handlers: cost guard.

use std::sync::Arc;

use super::super::{WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Cost ────────────────────────────────────────────────────────────────

/// `cost.get` — the cost guard's current state.
///
/// Reports both the limits and what has been consumed against them, so it is
/// possible to see *why* a guard tripped before clearing it.
pub(crate) async fn handle_cost_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let guard = &state.agents.cost_guard;
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "daily_spend_cents": guard.daily_spend_cents(),
            "daily_limit_cents": guard.daily_limit_cents,
            "hourly_actions": guard.hourly_action_count(),
            "hourly_action_limit": guard.hourly_action_limit,
            "exceeded": guard.is_exceeded(),
        }),
    )
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
}
