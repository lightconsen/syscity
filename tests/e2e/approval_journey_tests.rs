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

// ── The native desktop tools Stage A gated ──────────────────────────────────
//
// Stage A's fix was that every per-platform desktop-input tool now declares
// `requires_approval`. The unit tests pin the declaration and the gate; these
// pin the two journeys the plan asked for on top of them — a real turn that
// raises the prompt, and an unattended run that is refused with an audit line
// instead of moving the pointer.
//
// The tool is chosen from what the host actually registered rather than named
// as `macos_desktop_control`: the same gate backs all four platform tools, and
// naming one would reduce this to a macOS-only test that skips to nothing on
// the Linux CI runner. A host with no desktop toolset at all (a headless
// Linux box) skips *loudly* — there is nothing to gate, and a green test that
// never ran is worse than a red one.

/// The native desktop-input tool this host registered, if any.
///
/// Only the platform's own tool: `windows_powershell`/`applescript` run
/// arbitrary code and are gated differently (High risk, a different question
/// about what the call is).
fn native_desktop_tool(gateway: &Gateway) -> Option<&'static str> {
    const CANDIDATES: &[&str] = &[
        "macos_desktop_control",
        "windows_desktop_control",
        "linux_x11_desktop_control",
        "linux_wayland_desktop_control",
    ];
    let registry = gateway.tool_registry();
    CANDIDATES.iter().copied().find(|name| registry.has(name))
}

fn skip_with_no_desktop_tool() {
    eprintln!(
        "Skipping the desktop-approval E2E: this host registered no native desktop-input tool \
         (no macOS toolset, and no X11/Wayland display for the Linux ones). The gate it exercises \
         is unit-tested for all four tools in `src/tools/registry.rs`; the chat-level flow is \
         covered for every host by the `shell` journeys above."
    );
}

/// A chat turn that reaches a native desktop tool must raise an approval that
/// names the tool — and the answer must resolve the gate.
///
/// This is the hole Stage A closed, observed end to end: the model asks for the
/// tool directly (not through the `computer` aggregate), and the person is
/// asked before anything touches the pointer.
#[tokio::test]
#[serial]
async fn a_native_desktop_tool_raises_an_approval_naming_it() {
    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = new_test_gateway(config).await;
    let Some(tool) = native_desktop_tool(&gateway) else {
        skip_with_no_desktop_tool();
        return;
    };
    let mock = journey_mock(tool, json!({ "action": "inspect" }));
    register_mock_provider_with_model(&gateway.model_router(), mock.clone(), "mock-model").await;
    // No auto-approver: this test is the observer.
    start_gateway_and_wait_opts(port, gateway, false).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Inspect the desktop with the native desktop control tool.")
        .await;

    let payload = client
        .wait_for_event("approval.required", 30)
        .await
        .expect("a native desktop tool must gate its call");
    assert_eq!(
        payload["tool_name"].as_str(),
        Some(tool),
        "the prompt must name the tool being approved: {payload}"
    );
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(tool),
        "the prompt's text must name the tool too, got {message:?}"
    );

    // Deny rather than approve: the point is that the prompt resolves to an
    // answer that reaches the model, and denying also guarantees this test
    // never moves a real pointer on the machine running it.
    let approval_id = payload["approval_id"]
        .as_str()
        .expect("approval_id")
        .to_string();
    client
        .request("approvals.deny", json!({ "id": approval_id, "reason": "journey-desktop-deny" }))
        .await;

    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("the turn completes once the prompt is answered");

    let tool_contents: Vec<String> = mock
        .history()
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .collect();
    assert!(
        tool_contents
            .iter()
            .any(|c| c.contains("journey-desktop-deny")),
        "the person's answer must reach the model; tool messages: {tool_contents:?}"
    );
}

/// The same tool in a cron run has nobody to ask: it is refused before the
/// body runs, nothing is queued, and the refusal is in the audit log.
///
/// This drives the real unattended path rather than a hand-built context: the
/// cron executor runs the agent as the `system` actor under a `cron:`-prefixed
/// conversation, which is exactly the signature
/// `tools::ask_user::background_context_reason` recognises.
#[tokio::test]
#[serial]
async fn a_native_desktop_tool_in_a_cron_run_is_refused_and_audited() {
    use syscity::cron::cron::{CronJob, DeliveryMode, ExecutionTarget, Schedule, SessionTarget};

    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = new_test_gateway(config).await;
    let Some(tool) = native_desktop_tool(&gateway) else {
        skip_with_no_desktop_tool();
        return;
    };
    let state = gateway.state();

    // Ask for the tool once, then answer. The cache-check prompt is answered
    // out of band the way every other journey mock does it.
    let mock = {
        let tool = tool.to_string();
        MockProvider::new().with_callback(move |messages| {
            if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
                return ProviderMessage::assistant("NOCACHE");
            }
            if messages.iter().any(|m| m.role == Role::Tool) {
                return ProviderMessage::assistant("The desktop was not touched.");
            }
            ProviderMessage::assistant("Inspecting the desktop.").with_tool_calls(vec![ToolCall {
                id: "call_cron_desktop_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: tool.clone(),
                    arguments: json!({ "action": "inspect" }).to_string(),
                },
                index: None,
                result: None,
            }])
        })
    };
    register_mock_provider_with_model(&gateway.model_router(), mock.clone(), "mock-model").await;
    start_gateway_and_wait_opts(port, gateway, false).await;

    // The scheduler, wired to the default agent at startup.
    let scheduler = {
        let guard = state.scheduler.cron_scheduler.read().await;
        guard
            .clone()
            .expect("gateway start must install the cron scheduler")
    };
    let scheduler = scheduler.lock().await;
    let mut job = CronJob::new(
        "journey-desktop-gate",
        "desktop gate job",
        Schedule::Cron {
            expression: "0 0 1 1 *".to_string(),
            timezone: None,
            stagger_ms: None,
        },
        ExecutionTarget::agent("Inspect the desktop with the native desktop control tool."),
    );
    job.session = SessionTarget::Isolated;
    job.delivery = DeliveryMode::None;
    scheduler.add_job(job).await.expect("add job");
    scheduler
        .trigger_job("journey-desktop-gate")
        .await
        .expect("trigger job");
    drop(scheduler);

    // Wait for the refusal to reach the model. That is the evidence the tool
    // call actually happened and was gated (a tool that was never called
    // produces no tool message at all).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let refusal = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cron run never reached the desktop tool"
        );
        let found = mock
            .history()
            .iter()
            .flat_map(|req| req.messages.iter())
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .find(|c| c.contains("requires approval") || c.contains("no human is present"));
        if let Some(found) = found {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        refusal.contains("no human is present"),
        "an unattended run must be told why, in the terms the plan names: {refusal:?}"
    );

    // Nothing was queued: a background context must not leave a prompt waiting
    // for a person who is not there.
    let pending = state
        .tools
        .approval_queue
        .list_pending(Default::default())
        .await;
    assert!(
        pending.is_empty(),
        "an unattended context must not queue an approval: {pending:?}"
    );

    // And the refusal is audited against the tool, not only in a tool result
    // nobody reads.
    let entries = state.auth.audit_log.recent(200).await;
    let audited = entries
        .iter()
        .any(|e| e.target == tool && !e.allowed && e.description.to_lowercase().contains("deny"));
    assert!(
        audited,
        "the denial must be audited; recent targets: {:?}",
        entries
            .iter()
            .map(|e| (e.target.clone(), e.allowed, e.description.clone()))
            .collect::<Vec<_>>()
    );
}
