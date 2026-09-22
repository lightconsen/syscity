//! End-to-end tests for GoalPlanner (Cognition) routing in the Agent.
//!
//! These verify that messages matching `is_complex_task()` are routed through
//! `GoalPlanner::achieve()` instead of falling directly into the normal chat
//! path or `ComputerUseLoop`.
//!
//! Run:
//!   cargo test --test e2e_test goal_planner -- --nocapture

use super::*;

// ── Mock Provider for GoalPlanner E2E ───────────────────────────────────────

/// Build a MockProvider that handles the full GoalPlanner lifecycle.
///
/// 1. NOCACHE cache-check prompts → returns "NOCACHE".
/// 2. GoalPlanner decomposition requests (system prompt contains
///    "task-decomposition engine") → returns a static JSON array of subtasks.
/// 3. Everything else → returns a generic completion.
fn planner_mock_provider(subtasks_json: &str) -> MockProvider {
    let subtasks = subtasks_json.to_string();
    MockProvider::new().with_callback(move |messages| {
        eprintln!("[MOCK] received {} messages", messages.len());
        for (i, m) in messages.iter().enumerate() {
            eprintln!(
                "[MOCK] msg[{}] role={:?} content={}",
                i,
                m.role,
                &m.content[..m.content.len().min(80)]
            );
        }

        // 1. Cache check
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            eprintln!("[MOCK] -> NOCACHE");
            return ProviderMessage::assistant("NOCACHE");
        }

        // 2. Decomposition request from GoalPlanner
        let is_decompose = messages
            .iter()
            .any(|m| m.role == Role::System && m.content.contains("task-decomposition engine"));
        if is_decompose {
            eprintln!("[MOCK] -> DECOMPOSE (returning subtasks JSON)");
            return ProviderMessage::assistant(subtasks.clone());
        }

        // 3. Fallback (summary / normal chat)
        eprintln!("[MOCK] -> FALLBACK");
        ProviderMessage::assistant("GoalPlanner completed the task successfully.")
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// A message containing the keyword "deploy" should be routed through
/// GoalPlanner.  The mock returns a single no-op subtask so that the
/// execution finishes instantly.  We verify that the final chat response
/// contains the GoalPlanner result format ("Goal:" / "Success:").
#[tokio::test]
#[serial]
#[ignore = "GoalPlanner chat auto-routing (is_complex_task) was removed in c08288b; these tests verify the old heuristic routing"]
async fn goal_planner_triggered_by_complex_task_keyword() {
    let subtasks = r#"[
        {
            "id": "noop",
            "description": "No-op wait",
            "dependencies": [],
            "action": {"wait": {"milliseconds": 0}},
            "max_retries": 1
        }
    ]"#;

    start_test_gateway_with_mock(40500, planner_mock_provider(subtasks)).await;

    let mut client = FrontendSimulator::connect(40500).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    // "deploy" is in the `is_complex_task()` keyword list.
    client
        .send_chat(&sid, "Deploy test config to /tmp/syscity-planner-e2e")
        .await;

    let result = timeout(Duration::from_secs(30), async {
        while let Some(msg) = client.read.next().await {
            let msg = msg.unwrap();
            if let Message::Text(text) = msg {
                eprintln!("[EVENT] {}", text);
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

    assert!(
        response.contains("Goal:") || response.contains("Success:"),
        "Expected GoalPlanner result format in chat.final, got: {}",
        response
    );
}

/// When the GoalPlanner mock returns a multi-step DAG with dependencies,
/// the Agent should still produce a `chat.final` containing a summary.
#[tokio::test]
#[serial]
#[ignore = "GoalPlanner chat auto-routing (is_complex_task) was removed in c08288b; these tests verify the old heuristic routing"]
async fn goal_planner_multi_step_dag() {
    let subtasks = r#"[
        {
            "id": "step-1",
            "description": "First step",
            "dependencies": [],
            "action": {"wait": {"milliseconds": 0}},
            "max_retries": 1
        },
        {
            "id": "step-2",
            "description": "Second step",
            "dependencies": ["step-1"],
            "action": {"wait": {"milliseconds": 0}},
            "max_retries": 1
        }
    ]"#;

    start_test_gateway_with_mock(40501, planner_mock_provider(subtasks)).await;

    let mut client = FrontendSimulator::connect(40501).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    // "configure" is also in `is_complex_task()`.
    client
        .send_chat(&sid, "Configure the system for testing")
        .await;

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

    assert!(
        response.contains("Goal:") || response.contains("Success:"),
        "Expected GoalPlanner result format in chat.final, got: {}",
        response
    );
}

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

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");

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

/// The GoalPlanner should execute a ToolCall subtask via the ToolRegistry
/// when the decomposition emits a `tool_call` action targeting a device tool.
///
/// This verifies the full GoalPlanner + ToolCall + device orchestration path:
/// 1. Device tools are registered in ToolRegistry via Gateway::with_devices()
/// 2. A device-related chat message triggers GoalPlanner (is_complex_task)
/// 3. The mock LLM returns subtasks containing a `tool_call` action
/// 4. TaskExecutor dispatches the ToolCall through ToolRegistry
/// 5. The device tool executes, returning a result
/// 6. GoalPlanner produces a chat.final with "Goal:" / "Success:" format
#[tokio::test]
#[serial]
#[ignore = "Requires device driver types that have been removed"]
async fn goal_planner_tool_call_device() {
    // Test skipped: device driver system (MockCapability, MockDeviceDriver,
    // etc.) has been removed from the codebase.
}
