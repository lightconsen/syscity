use super::*;

/// Helper: execute a slash command with arguments via `commands.execute`.
///
/// `FrontendSimulator::execute_command` only passes the `command` field,
/// but goal and other argument-driven commands expect `params.args`.
async fn exec_goal(client: &mut FrontendSimulator, args: &str) -> serde_json::Value {
    client
        .request("commands.execute", json!({"command": "goal", "args": args}))
        .await
}

/// Poll `/goal list` until `goal_id` no longer appears (running or
/// suspended), giving a cancelled runner time to delete its checkpoint.
async fn wait_goal_cleaned_up(client: &mut FrontendSimulator, goal_id: &str) {
    for _ in 0..40 {
        let list = exec_goal(client, "list").await;
        let text = list
            .get("payload")
            .and_then(|p| p.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !text.contains(goal_id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("goal {} persisted state not cleaned up after cancel", goal_id);
}

/// Cancel a goal and wait for its persisted state to be gone.
///
/// `cancel` only fires the runner's token; the runner deletes its checkpoint
/// asynchronously (it may be mid-round), so the real `~/.syscity/goals` store
/// can briefly still list it. Wait rather than racing runtime teardown.
async fn cancel_goal_and_wait(client: &mut FrontendSimulator, goal_id: &str) {
    let cancel = exec_goal(client, &format!("cancel {}", goal_id)).await;
    assert!(
        cancel.get("ok").and_then(|v| v.as_bool()) == Some(true),
        "cancel failed: {:?}",
        cancel
    );
    wait_goal_cleaned_up(client, goal_id).await;
}

/// Build a MockProvider that handles both goal parsing and sub-agent execution.
///
/// - Cache checks ("NOCACHE") → "NOCACHE"
/// - Goal parsing (system prompt contains "goal analyzer") → JSON plan with
///   `model_override: "mock-model"` so the runner resolves correctly. The
///   condition is a 2-second *failing* shell check so the goal never passes
///   in round 1 — the list/cancel tests need it to stay in `goal_cancellers`
///   long enough to be observed. (`--max-rounds` from the test CLI overrides
///   the JSON `max_rounds`.)
/// - Sub-agent execution → text response ("Task completed.")
fn goal_mock_provider() -> MockProvider {
    MockProvider::new().with_callback(|messages| {
        // Cache-check prompt is a single user message asking about caching.
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }

        // Goal parsing call: system prompt contains "goal analyzer".
        let is_goal_parse = messages.iter().any(|m| m.content.contains("goal analyzer"));
        if is_goal_parse {
            return ProviderMessage::assistant(
                r#"{"description":"test goal","conditions":[{"type":"exit_code","command":"sleep 2; exit 1","expected":0}],"max_rounds":1,"model_override":"mock-model"}"#,
            );
        }

        // Sub-agent execution: return text so the agent finishes immediately.
        ProviderMessage::assistant("Task completed.")
    })
}

/// Start a test gateway for goal tests with proper mock router setup.
async fn start_goal_test_gateway(port: u16) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = new_test_gateway(config).await;

    let router = gateway.model_router();
    // Register the mock provider and its owned model; the default is already
    // "mock-model" (set on config.model above).
    register_mock_provider_with_model(&router, goal_mock_provider(), "mock-model").await;
    router
        .switch_default_model("mock-model")
        .await
        .expect("Failed to set default model");

    start_gateway_and_wait(port, gateway).await;
}

#[tokio::test]
#[serial]
async fn test_goal_full_lifecycle() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    let resp = exec_goal(&mut client, "write tests --max-rounds 1").await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let goal_id = resp
        .get("payload")
        .and_then(|p| p.get("goal_id"))
        .and_then(|v| v.as_str())
        .expect("goal_id")
        .to_string();

    // Wait for goal.started event.
    let started = client
        .wait_for_event("goal.progress", 15)
        .await
        .expect("Expected goal.started event");
    let inner = started.get("event").and_then(|v| v.as_object());
    assert!(inner.is_some(), "Expected nested goal.started event");
    assert_eq!(
        inner.and_then(|o| o.get("event")).and_then(|v| v.as_str()),
        Some("goal.started")
    );

    // Wait for goal.retry event (first round feedback).
    let retry = client
        .wait_for_event("goal.progress", 15)
        .await
        .expect("Expected goal.retry event");
    let retry_inner = retry.get("event").and_then(|v| v.as_object());
    assert!(retry_inner.is_some(), "Expected nested goal.retry event");
    assert_eq!(
        retry_inner
            .and_then(|o| o.get("event"))
            .and_then(|v| v.as_str()),
        Some("goal.retry")
    );

    // Wait for goal.check event.
    let check = client
        .wait_for_event("goal.progress", 15)
        .await
        .expect("Expected goal.check event");
    let check_inner = check.get("event").and_then(|v| v.as_object());
    assert!(check_inner.is_some(), "Expected nested goal.check event");
    assert_eq!(
        check_inner
            .and_then(|o| o.get("event"))
            .and_then(|v| v.as_str()),
        Some("goal.check")
    );

    // Wait for terminal event (goal.done or goal.aborted).
    let terminal = client
        .wait_for_event("goal.progress", 15)
        .await
        .expect("Expected terminal goal event");
    let term_inner = terminal.get("event").and_then(|v| v.as_object());
    assert!(term_inner.is_some(), "Expected nested terminal event");
    let term_type = term_inner
        .and_then(|o| o.get("event"))
        .and_then(|v| v.as_str());
    assert!(
        term_type == Some("goal.done") || term_type == Some("goal.aborted"),
        "Expected goal.done or goal.aborted, got: {:?}",
        term_type
    );

    // Cleanup: max_rounds leaves a persisted checkpoint — discard it.
    cancel_goal_and_wait(&mut client, &goal_id).await;
}

#[tokio::test]
#[serial]
async fn test_goal_command_starts_and_completes() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    // Send /goal command.
    let resp = exec_goal(&mut client, "write tests --max-rounds 1").await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));

    // Expect goal.started event.
    let started = client
        .wait_for_event("goal.progress", 15)
        .await
        .expect("Expected goal.progress event");
    let inner = started.get("event").and_then(|v| v.as_object());
    assert!(inner.is_some(), "Expected nested goal.started event");
    let event_type = inner.and_then(|o| o.get("event")).and_then(|v| v.as_str());
    assert_eq!(event_type, Some("goal.started"));

    // Cleanup: cancel the still-running goal.
    let goal_id = resp
        .get("payload")
        .and_then(|p| p.get("goal_id"))
        .and_then(|v| v.as_str())
        .expect("goal_id")
        .to_string();
    cancel_goal_and_wait(&mut client, &goal_id).await;
}

#[tokio::test]
#[serial]
async fn test_goal_command_creates_goal_id() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    let resp = exec_goal(&mut client, "write tests --max-rounds 1").await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));

    let payload = resp.get("payload").and_then(|v| v.as_object()).cloned();
    assert!(payload.is_some(), "Expected payload in response: {:?}", resp);
    let goal_id = payload
        .as_ref()
        .and_then(|p| p.get("goal_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    assert!(goal_id.is_some(), "Expected goal_id in response payload");
    let gid = goal_id.unwrap();
    assert!(gid.starts_with("goal_"), "goal_id should start with 'goal_'");

    // Cleanup: cancel the still-running goal.
    cancel_goal_and_wait(&mut client, &gid).await;
}

#[tokio::test]
#[serial]
async fn test_goal_list_shows_active_goals() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    // Should show no goals initially.
    let list_resp = exec_goal(&mut client, "list").await;
    assert!(list_resp.get("ok").and_then(|v| v.as_bool()) == Some(true));

    // Start a goal.
    let resp = exec_goal(&mut client, "write tests --max-rounds 1").await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));

    // List should show the active goal.
    let list_resp2 = exec_goal(&mut client, "list").await;
    assert!(list_resp2.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let text = list_resp2
        .get("payload")
        .and_then(|p| p.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains("goal_"), "Expected goal ID in list response: {}", text);

    // Cleanup: cancel the still-running goal.
    let goal_id = resp
        .get("payload")
        .and_then(|p| p.get("goal_id"))
        .and_then(|v| v.as_str())
        .expect("goal_id")
        .to_string();
    cancel_goal_and_wait(&mut client, &goal_id).await;
}

#[tokio::test]
#[serial]
async fn test_goal_cancel_aborts_goal() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    // Start a goal.
    let resp = exec_goal(&mut client, "write tests --max-rounds 5").await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let goal_id = resp
        .get("payload")
        .and_then(|p| p.get("goal_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Cancel it by ID.
    let cancel_resp = exec_goal(&mut client, &format!("cancel {}", goal_id)).await;
    assert!(
        cancel_resp.get("ok").and_then(|v| v.as_bool()) == Some(true),
        "Cancel failed: {:?}",
        cancel_resp
    );
    let text = cancel_resp
        .get("payload")
        .and_then(|p| p.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains(&goal_id), "Cancel response should mention goal ID");
    wait_goal_cleaned_up(&mut client, &goal_id).await;
}

#[tokio::test]
#[serial]
async fn test_goal_cancel_unknown_id_returns_error() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let _sid = client.create_session().await;

    let cancel_resp = exec_goal(&mut client, "cancel nonexistent_goal_xyz").await;
    assert!(
        cancel_resp.get("ok").and_then(|v| v.as_bool()) == Some(false),
        "Expected error for unknown goal ID"
    );
}

#[tokio::test]
#[serial]
async fn test_goal_empty_description_returns_error() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let _sid = client.create_session().await;

    let resp = exec_goal(&mut client, "  ").await;
    assert!(
        resp.get("ok").and_then(|v| v.as_bool()) == Some(false),
        "Expected error for empty goal description"
    );
}

/// Wait for goal.progress events until `goal.aborted` arrives; returns the
/// inner aborted event payload.
async fn wait_for_goal_abort(client: &mut FrontendSimulator) -> serde_json::Value {
    for _ in 0..10 {
        if let Some(payload) = client.wait_for_event("goal.progress", 30).await {
            let inner = payload.get("event").cloned().unwrap_or_default();
            if inner.get("event").and_then(|v| v.as_str()) == Some("goal.aborted") {
                return inner;
            }
        }
    }
    panic!("goal.aborted event did not arrive");
}

#[tokio::test]
#[serial]
async fn test_goal_blocked_persists_reason_and_resumes() {
    let port = free_port();
    start_goal_test_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    // The mock condition (`sleep 2; exit 1`) fails identically every round,
    // so the loop detector blocks the goal at round 3.
    let resp = exec_goal(&mut client, "write tests --max-rounds 5").await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let goal_id = resp
        .get("payload")
        .and_then(|p| p.get("goal_id"))
        .and_then(|v| v.as_str())
        .expect("goal_id")
        .to_string();

    let aborted = wait_for_goal_abort(&mut client).await;
    assert_eq!(
        aborted
            .get("blocked_reason")
            .and_then(|b| b.get("code"))
            .and_then(|c| c.as_str()),
        Some("loop-detected"),
        "aborted event must carry the structured reason: {:?}",
        aborted
    );

    // Blocked goal persists: /goal list shows it as suspended with the reason.
    let list = exec_goal(&mut client, "list").await;
    let text = list
        .get("payload")
        .and_then(|p| p.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains("Suspended Goals"), "expected suspended section: {}", text);
    assert!(text.contains(&goal_id), "goal must be listed: {}", text);
    assert!(text.contains("loop-detected"), "reason must be listed: {}", text);

    // Explicit resume re-arms the runner.
    let resume = exec_goal(&mut client, &format!("resume {}", goal_id)).await;
    assert!(
        resume.get("ok").and_then(|v| v.as_bool()) == Some(true),
        "resume failed: {:?}",
        resume
    );
    let started = client
        .wait_for_event("goal.progress", 30)
        .await
        .expect("resumed goal emits events");
    let inner = started.get("event").cloned().unwrap_or_default();
    assert_eq!(inner.get("event").and_then(|v| v.as_str()), Some("goal.started"));

    // Cleanup: cancel discards the live runner (or the persisted file).
    cancel_goal_and_wait(&mut client, &goal_id).await;
}

#[tokio::test]
#[serial]
async fn test_goals_not_auto_resumed_on_restart() {
    // gw1 starts a goal that fails forever; once its first checkpoint is
    // saved, a second gateway (same HOME, another port) must list it as
    // suspended — not running — proving startup does not re-arm it.
    let port1 = free_port();
    let port2 = free_port();
    start_goal_test_gateway(port1).await;
    let mut client1 = FrontendSimulator::connect(port1).await;

    let sid = client1.create_session().await;
    client1.subscribe(vec![sid.clone()]).await;

    let resp = exec_goal(&mut client1, "write tests --max-rounds 5").await;
    assert!(resp.get("ok").and_then(|v| v.as_bool()) == Some(true));
    let goal_id = resp
        .get("payload")
        .and_then(|p| p.get("goal_id"))
        .and_then(|v| v.as_str())
        .expect("goal_id")
        .to_string();

    // Wait for the first failed round — the checkpoint exists on disk now.
    loop {
        let payload = client1
            .wait_for_event("goal.progress", 30)
            .await
            .expect("goal.check event");
        let inner = payload.get("event").cloned().unwrap_or_default();
        if inner.get("event").and_then(|v| v.as_str()) == Some("goal.check") {
            break;
        }
    }

    // The second gateway must NOT auto-resume the persisted goal.
    start_goal_test_gateway(port2).await;
    let mut client2 = FrontendSimulator::connect(port2).await;
    let _sid2 = client2.create_session().await;
    let list = exec_goal(&mut client2, "list").await;
    let payload = list.get("payload").cloned().unwrap_or_default();
    let running: Vec<String> = payload
        .get("goals")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(!running.contains(&goal_id), "gw2 must not run the resumed goal: {:?}", running);
    let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        text.contains(&goal_id) && text.contains("Suspended"),
        "gw2 must list the goal as suspended: {}",
        text
    );

    // Cleanup: cancel on gw1 (cancels the live runner or discards the file).
    cancel_goal_and_wait(&mut client1, &goal_id).await;
}
