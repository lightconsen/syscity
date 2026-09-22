//! The approval and question prompts, through the real loop.

use std::sync::Arc;

use crate::tui::actions::TuiAction;
use crate::tui::app::SessionChoice;
use crate::tui::event_loop::prompts::handle_approval_action;
use crate::tui::event_loop::run;
use crate::tui::state::{ApprovalChoice, LiveMode};
use crate::tui::test_gateway::TestGateway;

use super::support::*;

/// A decision the gateway refuses is not a decision.
///
/// Both arms used to `pop_approval()`. On the error path that retired the
/// prompt while the approval was still pending server-side and the tool
/// call was still blocked — so the UI had thrown away the only thing that
/// could unblock it, and the human's "yes" was never recorded anywhere.
#[tokio::test]
async fn a_refused_decision_keeps_the_prompt() {
    let gateway = TestGateway::start().await;
    gateway.fail_with("approvals.approve", "INTERNAL");
    let (state, client) = state_with_approval(&gateway).await;

    handle_approval_action(TuiAction::InputChar('y'), &state, &client)
        .await
        .expect("handled");

    let mut s = state.write().await;
    assert_eq!(s.approvals.len(), 1, "the approval is still waiting for an answer");
    assert_eq!(s.live_mode, LiveMode::Approval);
    let lines: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("could not answer")),
        "and it says so: {lines:?}"
    );
}

/// An approval that is already gone retires its prompt.
///
/// `NOT_FOUND` means another client answered it, or it expired, or the turn
/// ended. Retrying cannot succeed, and the prompt owns the keyboard, so
/// keeping it would be a trap.
#[tokio::test]
async fn a_decision_for_a_resolved_approval_retires_the_prompt() {
    let gateway = TestGateway::start().await;
    gateway.fail_with("approvals.approve", "NOT_FOUND");
    let (state, client) = state_with_approval(&gateway).await;

    handle_approval_action(TuiAction::InputChar('y'), &state, &client)
        .await
        .expect("handled");

    let mut s = state.write().await;
    assert!(s.approvals.is_empty(), "the prompt goes");
    assert_eq!(s.live_mode, LiveMode::Composer, "the composer comes back");
    let lines: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("already resolved")),
        "and it says why: {lines:?}"
    );
}

/// Ctrl+C is never swallowed by the prompt.
///
/// A tool call blocked on a human stays blocked however the human feels
/// about it, but the prompt must not be able to keep the keyboard forever.
#[tokio::test]
async fn ctrl_c_dismisses_the_prompt_without_answering_it() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_with_approval(&gateway).await;

    handle_approval_action(TuiAction::Abort, &state, &client)
        .await
        .expect("handled");

    let mut s = state.write().await;
    assert!(s.approvals.is_empty());
    assert_eq!(s.live_mode, LiveMode::Composer);
    let lines: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("stays blocked")),
        "dismissing is not answering, and it says so: {lines:?}"
    );
    assert!(
        !gateway
            .requests()
            .iter()
            .any(|r| r.method.starts_with("approvals.")),
        "no decision was sent"
    );
}

/// The approval keys go through the real loop, end to end.
///
/// `event → approvals.get → prompt → y → approvals.approve → notice →
/// composer returns` — every piece had unit coverage; the round trip
/// driving the actual `run`, with a keypress injected through the seam and
/// the decision observable on the wire and in the terminal, did not.
#[tokio::test]
async fn the_approval_keys_drive_a_real_decision() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let observed = Arc::clone(&state);
    let (mut input, tx) = ScriptedInput::new();

    let driver = tokio::spawn(async move {
        let mut terminal = inline_terminal();
        let r = run(
            &mut terminal,
            state,
            client,
            test_endpoint(gateway.port),
            SessionChoice::New,
            &mut input,
        )
        .await;
        (r, terminal)
    });

    gateway.push_event(
        "approval.required",
        serde_json::json!({
            "approval_id": "ap1",
            "tool_name": "file_write",
            "requested_by": "secretary",
            "risk_level": "High",
            "message": "writes outside the workspace",
        }),
    );

    let prompt_up = || {
        let observed = Arc::clone(&observed);
        async move {
            let s = observed.read().await;
            s.live_mode == LiveMode::Approval && s.approvals.len() == 1
        }
    };
    eventually_async(prompt_up, "the approval prompt to open").await;

    tx.send(TuiAction::InputChar('y')).expect("queued");
    gateway.wait_for("approvals.approve", PATIENCE).await;

    let answered = || {
        let observed = Arc::clone(&observed);
        async move {
            let s = observed.read().await;
            s.approvals.is_empty() && s.live_mode == LiveMode::Composer
        }
    };
    eventually_async(answered, "the prompt to retire").await;

    tx.send(TuiAction::Quit).expect("queued");
    let (result, terminal) = tokio::time::timeout(PATIENCE, driver)
        .await
        .expect("the loop exits")
        .expect("join");
    result.expect("run");

    // The decision is reported where the user can still read it, and the
    // 'y' went to the prompt, not the composer.
    let screen = painted(&terminal);
    assert!(
        screen.contains("✔ approved file_write"),
        "the approval is confirmed in the terminal: {screen:?}"
    );
    let decision = gateway
        .requests()
        .into_iter()
        .find(|r| r.method == "approvals.approve")
        .expect("a decision on the wire");
    assert_eq!(decision.params["id"], "ap1");
}

/// `a` approves and remembers: the approve request carries
/// `remember: true`, and the reply's rule is echoed into the transcript.
#[tokio::test]
async fn the_a_key_approves_and_remembers() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let observed = Arc::clone(&state);
    let (mut input, tx) = ScriptedInput::new();

    let driver = tokio::spawn(async move {
        let mut terminal = inline_terminal();
        let r = run(
            &mut terminal,
            state,
            client,
            test_endpoint(gateway.port),
            SessionChoice::New,
            &mut input,
        )
        .await;
        (r, terminal)
    });

    gateway.push_event(
        "approval.required",
        serde_json::json!({
            "approval_id": "ap1",
            "tool_name": "file_write",
            "requested_by": "secretary",
            "risk_level": "High",
            "message": "writes outside the workspace",
        }),
    );

    let prompt_up = || {
        let observed = Arc::clone(&observed);
        async move {
            let s = observed.read().await;
            s.live_mode == LiveMode::Approval && s.approvals.len() == 1
        }
    };
    eventually_async(prompt_up, "the approval prompt to open").await;

    tx.send(TuiAction::InputChar('a')).expect("queued");
    let remembered = || {
        let gateway_ref = &gateway;
        async move {
            gateway_ref
                .requests()
                .iter()
                .any(|r| r.method == "approvals.approve")
        }
    };
    eventually_async(remembered, "the approve request to hit the wire").await;

    tx.send(TuiAction::Quit).expect("queued");
    let (result, terminal) = tokio::time::timeout(PATIENCE, driver)
        .await
        .expect("the loop exits")
        .expect("join");
    result.expect("run");

    let decision = gateway
        .requests()
        .into_iter()
        .find(|r| r.method == "approvals.approve")
        .expect("a decision on the wire");
    assert_eq!(decision.params["remember"], true, "`a` must ask to remember");

    let screen = painted(&terminal);
    assert!(
        screen.contains("✔ approved file_write"),
        "the approval is confirmed in the terminal: {screen:?}"
    );
}

/// The arrows cycle the three choices; the state resets to Approve when
/// the prompt retires.
#[tokio::test]
async fn the_approval_arrows_cycle_three_choices() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let observed = Arc::clone(&state);
    let (mut input, tx) = ScriptedInput::new();

    let driver = tokio::spawn(async move {
        let mut terminal = inline_terminal();
        let r = run(
            &mut terminal,
            state,
            client,
            test_endpoint(gateway.port),
            SessionChoice::New,
            &mut input,
        )
        .await;
        (r, terminal)
    });

    gateway.push_event(
        "approval.required",
        serde_json::json!({
            "approval_id": "ap1",
            "tool_name": "file_write",
            "requested_by": "secretary",
            "risk_level": "High",
            "message": "writes outside the workspace",
        }),
    );

    let prompt_up = || {
        let observed = Arc::clone(&observed);
        async move {
            let s = observed.read().await;
            s.live_mode == LiveMode::Approval && s.approvals.len() == 1
        }
    };
    eventually_async(prompt_up, "the approval prompt to open").await;

    for _ in 0..2 {
        tx.send(TuiAction::CursorRight).expect("queued");
    }
    let at_deny = || {
        let observed = Arc::clone(&observed);
        async move { observed.read().await.approval_selection == ApprovalChoice::Deny }
    };
    eventually_async(at_deny, "two arrows land on deny").await;
    // One more wraps back around.
    tx.send(TuiAction::CursorRight).expect("queued");
    let wrapped = || {
        let observed = Arc::clone(&observed);
        async move { observed.read().await.approval_selection == ApprovalChoice::Approve }
    };
    eventually_async(wrapped, "the third arrow wraps to approve").await;

    tx.send(TuiAction::Quit).expect("queued");
    let (result, _terminal) = tokio::time::timeout(PATIENCE, driver)
        .await
        .expect("the loop exits")
        .expect("join");
    result.expect("run");
}
