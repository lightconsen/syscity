use super::*;

/// Simple provider: answer directly. `MockProvider::stream` always attaches a
/// usage-bearing final chunk, so every parent turn produces `agent.usage`
/// events.
fn usage_mock_provider() -> MockProvider {
    MockProvider::new().with_callback(|messages| {
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        ProviderMessage::assistant("all done")
    })
}

/// End-to-end: a chat turn publishes per-round usage over WS as `agent.usage`,
/// carrying the session and a real total, so clients can run a live token
/// counter during the turn instead of waiting for `chat.final`.
#[tokio::test]
#[serial]
async fn agent_usage_flows_to_the_ws_client() {
    let port = free_port();
    start_test_gateway_with_mock(port, usage_mock_provider()).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    client.send_chat(&sid, "say something").await;

    let payload = client
        .wait_for_event("agent.usage", 30)
        .await
        .expect("expected an agent.usage event during the turn");
    assert_eq!(payload["session_id"], sid, "the event names the session");
    let total = payload["usage"]["total_tokens"]
        .as_u64()
        .expect("total_tokens");
    assert!(total > 0, "the mock reports a real total: {total}");
}

/// Provider that delegates, then aggregates.
///
/// - Cache-check prompts ("NOCACHE") → "NOCACHE"
/// - A message naming the child task ("finish this") is the CHILD's own turn
///   (the test gateway has a `default` agent, so the child really runs the
///   mock) → the child finishes without delegating further, keeping the tree
///   at one level.
/// - The turn after the wake message → final answer.
/// - After the spawn result is back → final answer for the first turn.
/// - Otherwise → tool call `delegate spawn`.
fn delegate_mock_provider() -> MockProvider {
    MockProvider::new().with_callback(|messages| {
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        if messages
            .iter()
            .any(|m| m.content.contains("No agent configured for delegation"))
        {
            return ProviderMessage::assistant("Aggregated. Done!");
        }
        if messages.iter().any(|m| m.content.contains("finish this")) {
            return ProviderMessage::assistant("child finished the task");
        }
        let has_tool_result = messages.iter().any(|m| m.role == Role::Tool);
        if has_tool_result {
            return ProviderMessage::assistant("Spawned. I'll wait for it.");
        }
        ProviderMessage::assistant("I'll delegate that.").with_tool_calls(vec![ToolCall {
            id: "call_delegate_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "delegate".to_string(),
                arguments: r#"{"action":"spawn","task":{"prompt":"finish this"}}"#.to_string(),
            },
            index: None,
            result: None,
        }])
    })
}

/// End-to-end: a `delegate` spawn publishes `delegation.updated` to the client
/// subscribed to the ROOT USER session — `running` on creation, `completed`
/// with usage and a computed duration on terminal status — after the operator
/// approves the high-risk call through the WS approval flow, exactly as the
/// TUI would.
#[tokio::test]
#[serial]
async fn delegate_spawn_pushes_task_rows_with_the_root_session() {
    let port = free_port();
    start_test_gateway_with_mock(port, delegate_mock_provider()).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    client.send_chat(&sid, "please delegate a task").await;

    // `delegate` is a high-risk call; the default permission mode sends it to
    // the approval flow. Approve it the way the TUI's `y` does.
    let approval = client
        .wait_for_event("approval.required", 30)
        .await
        .expect("expected approval.required for the delegate call");
    assert_eq!(approval["tool_name"], "delegate");
    let approval_id = approval["approval_id"].as_str().unwrap().to_string();
    let resp = client
        .request("approvals.approve", json!({ "id": approval_id }))
        .await;
    assert!(
        resp.get("ok").and_then(|v| v.as_bool()) == Some(true),
        "approvals.approve failed: {:?}",
        resp.get("error")
    );

    // The child row is created (running) and then completes (the child runs
    // the default agent against the mock). Both land as `delegation.updated`
    // on the root user session — the routing this feature exists for.
    let running = client
        .wait_for_event("delegation.updated", 30)
        .await
        .expect("expected delegation.updated (running)");
    assert_eq!(running["session_id"], sid, "the event names the user session");
    assert_eq!(running["status"], "running");
    assert_eq!(running["title"], "finish this");
    assert_eq!(running["duration_ms"], json!(null));
    let task_id = running["task_id"].as_str().unwrap().to_string();

    // Every row write notifies (create, the started event, each usage round),
    // so several `running` events may arrive before the terminal one; the TUI
    // upserts through exactly this churn.
    let mut completed = None;
    for _ in 0..20 {
        let Some(payload) = client.wait_for_event("delegation.updated", 10).await else {
            break;
        };
        if payload["status"] == "completed" || payload["status"] == "failed" {
            completed = Some(payload);
            break;
        }
    }
    let completed = completed.expect("expected a terminal delegation.updated");
    assert_eq!(completed["session_id"], sid);
    assert_eq!(completed["task_id"], task_id, "the same row updates in place");
    assert_eq!(completed["status"], "completed");
    assert!(
        completed["duration_ms"].as_u64().is_some(),
        "terminal rows carry a computed duration"
    );
    assert!(
        completed["usage_tokens"].as_u64().unwrap_or(0) > 0,
        "the child's LLM rounds were accumulated onto the row: {completed}"
    );

    // The wake message re-runs the parent, which aggregates and finishes.
    let _ = client.wait_for_event("chat.final", 30).await;
}
