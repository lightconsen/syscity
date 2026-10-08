//! Approval & permission-rule journey tests.
//!
//! The tool journeys in `tool_chat_tests` run with `spawn_auto_approver`
//! polling in the background, so the approval gate is *exercised* on every
//! journey but never *observed* — no test asserts that the
//! `approval.required` event arrives, that the turn actually suspends, or
//! that a denial blocks the tool body. These tests observe all three.
//!
//! Covered here:
//! 1. prompt → tool → `approval.required` → approve → tool runs → reply
//! 2. prompt → tool → `approval.required` → deny → tool body never runs,
//!    the denial reason reaches the model, the turn still completes
//! 3. a `[permissions].deny` rule blocks a tool outright (no approval event
//!    at all — deny is not a soft ask)
//! 4. a `[permissions].ask` rule forces the approval flow for a tool whose
//!    own posture would not have asked (read-only)

use super::*;

use std::path::PathBuf;
use syscity::tools::PermissionsConfig;

/// A MockProvider driving a two-turn conversation around an arbitrary tool
/// call. First turn requests `tool` with `arguments`; second turn answers
/// finally. Handles the NOCACHE cache-check prompt automatically.
fn journey_mock(tool: &str, arguments: serde_json::Value) -> MockProvider {
    let tool = tool.to_string();
    let arguments = arguments.to_string();
    MockProvider::new().with_callback(move |messages| {
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        if messages.iter().any(|m| m.role == Role::Tool) {
            return ProviderMessage::assistant("Done.");
        }
        ProviderMessage::assistant("I'll run that for you.").with_tool_calls(vec![ToolCall {
            id: "call_journey_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: tool.clone(),
                arguments: arguments.clone(),
            },
            index: None,
            result: None,
        }])
    })
}

/// Start a gateway without the auto-approver so the test can observe and
/// resolve approval requests itself.
async fn start_gatekeeperless_gateway(port: u16, mock: MockProvider) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock, "mock-model").await;
    start_gateway_and_wait_opts(port, gateway, false).await;
}

/// A gateway whose `[permissions]` rules are pre-loaded. `mode` stays default;
/// the rules carry the journey.
async fn start_rules_gateway(port: u16, mock: MockProvider, permissions: PermissionsConfig) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.permissions = permissions;
    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock, "mock-model").await;
    start_gateway_and_wait_opts(port, gateway, false).await;
}

/// Wait for the next `approval.required` event and return its id.
async fn wait_approval_required(client: &mut FrontendSimulator) -> String {
    let payload = client
        .wait_for_event("approval.required", 30)
        .await
        .expect("Expected the approval.required event");
    payload
        .get("approval_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .expect("approval.required must carry approval_id")
}

/// Case 1 — approve: the tool body runs only after the operator approves.
#[tokio::test]
#[serial]
async fn approval_required_then_approve_lets_the_tool_run() {
    let port = free_port();
    let sentinel = std::env::temp_dir().join(format!("syscity_appr_ok_{}.marker", port));
    let _ = std::fs::remove_file(&sentinel);

    let command = format!("echo approved-ran > '{}'", sentinel.display());
    start_gatekeeperless_gateway(port, journey_mock("shell", json!({ "command": command }))).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the shell tool to write the sentinel file.")
        .await;

    // The tool gates; the turn suspends. The sentinel must not exist while
    // the request is pending.
    let approval_id = wait_approval_required(&mut client).await;
    assert!(!sentinel.exists(), "tool body must not run before approval");

    client
        .request("approvals.approve", json!({ "id": approval_id }))
        .await;

    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("turn must complete after approval");
    assert!(sentinel.exists(), "approved tool body must have run");
}

/// Case 2 — deny: the turn completes, the model sees the denial reason, the
/// tool body never runs.
#[tokio::test]
#[serial]
async fn approval_deny_blocks_the_tool_body() {
    let port = free_port();
    let sentinel = std::env::temp_dir().join(format!("syscity_appr_deny_{}.marker", port));
    let _ = std::fs::remove_file(&sentinel);

    let command = format!("echo denied-ran > '{}'", sentinel.display());
    let mock = journey_mock("shell", json!({ "command": command }));
    start_gatekeeperless_gateway(port, mock.clone()).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the shell tool to write the sentinel file.")
        .await;

    let approval_id = wait_approval_required(&mut client).await;
    client
        .request("approvals.deny", json!({ "id": approval_id, "reason": "journey-deny" }))
        .await;

    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("the turn must still complete after a denial");

    // The tool body never ran…
    assert!(!sentinel.exists(), "denied tool body must not run");
    // …and the denial reason reached the model as the tool result.
    let tool_contents: Vec<String> = mock
        .history()
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .collect();
    assert!(
        tool_contents.iter().any(|c| c.contains("journey-deny")),
        "denial reason must reach the model; tool messages: {:?}",
        tool_contents
    );
}

/// Case 3 — a `[permissions].deny` rule blocks the tool outright: no
/// approval event is ever emitted (deny is not a soft ask), the denial
/// reason reaches the model, the body never runs.
#[tokio::test]
#[serial]
async fn permissions_deny_rule_blocks_without_an_approval_prompt() {
    let port = free_port();
    let sentinel = std::env::temp_dir().join(format!("syscity_rule_deny_{}.marker", port));
    let _ = std::fs::remove_file(&sentinel);

    let command = format!("echo rule-deny-ran > '{}'", sentinel.display());
    let mock = journey_mock("shell", json!({ "command": command }));
    start_rules_gateway(
        port,
        mock.clone(),
        PermissionsConfig {
            deny: vec!["shell".to_string()],
            ..Default::default()
        },
    )
    .await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the shell tool to write the sentinel file.")
        .await;

    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("the turn completes with the denial reaching the model");

    // No approval was requested at any point.
    let pending = client.request("approvals.list", json!({})).await;
    let approvals = pending
        .get("payload")
        .and_then(|r| r.get("approvals"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        approvals.is_empty(),
        "a deny rule must not raise an approval prompt: {approvals:?}"
    );

    assert!(!sentinel.exists(), "denied tool body must not run");
    let tool_contents: Vec<String> = mock
        .history()
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .collect();
    assert!(
        tool_contents.iter().any(|c| c.contains("deny rule")),
        "the rule's denial must reach the model; tool messages: {:?}",
        tool_contents
    );
}

/// Case 4 — a `[permissions].ask` rule forces the approval flow for a tool
/// that would never ask on its own: `grep` is read-only. The gate consults
/// the rule before the tool's own posture.
#[tokio::test]
#[serial]
async fn permissions_ask_rule_forces_approval_for_a_readonly_tool() {
    let port = free_port();

    // A fixture of its own rather than the machine's `/tmp`. The journey is
    // about an ask rule gating a read-only tool; pointing it at `/tmp` made the
    // test's runtime depend on whatever a developer's temp directory happens to
    // hold. On one such machine that was a browser profile's caches — a few
    // megabytes of matches, which the result filter scans at ~7s/MB, past the
    // 30s the assertion below allows. Held for the test's lifetime because the
    // mock only carries the path string.
    let fixture = tempfile::tempdir().expect("fixture dir");
    std::fs::write(fixture.path().join("notes.txt"), "anything goes here\n").expect("fixture file");

    let mock = journey_mock(
        "grep",
        json!({
            "pattern": "anything",
            "path": fixture.path().to_str().expect("fixture path is UTF-8"),
        }),
    );
    start_rules_gateway(
        port,
        mock,
        PermissionsConfig {
            ask: vec!["grep".to_string()],
            ..Default::default()
        },
    )
    .await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the grep tool to search for something.")
        .await;

    // The read-only tool must gate on the rule's account.
    let payload = client
        .wait_for_event("approval.required", 30)
        .await
        .expect("an ask rule must force the read-only tool through approval");
    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        message.contains("ask rule"),
        "the approval must cite the ask rule, got: {message}"
    );

    client
        .request("approvals.approve", json!({ "id": payload["approval_id"] }))
        .await;
    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("turn must complete after approval");
}
