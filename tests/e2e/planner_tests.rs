//! End-to-end tests for GoalPlanner (Cognition) fallback in the Agent.
//!
//! The `is_complex_task()` heuristic routing into `GoalPlanner::achieve()`
//! was removed (c08288b); with the planner unavailable a complex-looking task
//! must fall through to normal chat processing and still produce `chat.final`.
//!
//! Run:
//!   cargo test --test e2e_test goal_planner -- --nocapture

use super::*;

// ── Tests ───────────────────────────────────────────────────────────────────

/// If the GoalPlanner is unavailable (no computer adapter configured) the
/// complex task should fall through to normal processing and still produce
/// a `chat.final` event.
#[tokio::test]
#[serial]
async fn goal_planner_fallback_when_no_adapter() {
    // Build a config with computer explicitly disabled so the Agent
    // never gets a GoalPlanner field.
    let mut config = test_config(40502, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.computer.enabled = false;

    let gateway = new_test_gateway(config).await;

    let router = gateway.model_router();
    let mock = llm_mock_provider_for_streaming();
    register_mock_provider_with_model(&router, mock, "mock-model").await;

    start_gateway_and_wait(40502, gateway).await;

    let mut client = FrontendSimulator::connect(40502).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    client.send_chat(&sid, "Build and deploy the project").await;

    let result = timeout(Duration::from_secs(30), async {
        while let Some(msg) = client.read.next().await {
            let msg = msg.unwrap();
            if let Message::Text(text) = msg {
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(&text) {
                    if event.get("type").and_then(|v| v.as_str()) == Some("event") {
                        if event.get("event").and_then(|v| v.as_str()) == Some("chat.final") {
                            return event.get("payload").cloned();
                        }
                    }
                }
            }
        }
        None
    })
    .await;

    let payload = result
        .expect("Timed out waiting for chat.final")
        .expect("No chat.final event received");

    let response = payload
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Fallback to normal chat should still produce a response.
    assert!(!response.is_empty(), "Expected non-empty fallback response, got empty");
}
