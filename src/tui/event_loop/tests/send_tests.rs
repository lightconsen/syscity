//! Sending: the queue, `/retry`, and aborting a turn.

use std::sync::Arc;
use std::time::Duration;

use crate::tui::actions::TuiAction;
use crate::tui::app::SessionChoice;
use crate::tui::event_loop::events::handle_event;
use crate::tui::event_loop::keys::abort_or_quit;
use crate::tui::event_loop::send::send_message;
use crate::tui::event_loop::{retry_last_message, run};
use crate::tui::state::AppState;
use crate::tui::test_gateway::TestGateway;

use super::support::*;

/// A message typed mid-turn is queued, not sent alongside the first.
///
/// `send_message` had no `is_running` guard: Enter during a stream ran
/// `sessions.create`-less but `begin_run` + `chat.send` unconditionally, so
/// a second turn started on the same session in parallel with the first —
/// two streams interleaved into one transcript, and the second `final`
/// ending a run that was still going.
#[tokio::test]
async fn a_message_typed_mid_turn_is_queued() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    {
        let mut s = state.write().await;
        s.current_session = Some("s1".to_string());
        s.begin_run();
        s.set_input("and another thing".to_string());
    }

    send_message(&state, &mut client).await.expect("queued");

    {
        let s = state.read().await;
        assert!(s.is_running, "the first turn is untouched");
        assert_eq!(s.queued.len(), 1);
        assert_eq!(s.queued[0], "and another thing");
        assert!(s.input_buffer.is_empty(), "the composer is cleared");
    }
    assert!(!gateway.requests().iter().any(|r| r.method == "chat.send"), "nothing was sent");
}

/// A queued message goes out when the turn in front of it ends.
#[tokio::test]
async fn a_queued_message_is_sent_when_the_turn_ends() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    {
        let mut s = state.write().await;
        s.current_session = Some("s1".to_string());
        s.begin_run();
        s.queue_message("second".to_string());
    }

    handle_event(
        event("chat.final", serde_json::json!({ "session_id": "s1", "response": "first\n" })),
        &state,
        &mut client,
    )
    .await;

    let sent = gateway.wait_for("chat.send", PATIENCE).await;
    assert_eq!(sent["message"], "second");
    assert_eq!(sent["session_id"], "s1");
    assert!(state.read().await.queued.is_empty());
}

/// `/retry` with nothing running resends the last prompt through the real
/// loop, carrying the exact text that was sent the first time.
#[tokio::test]
async fn retry_resends_the_last_prompt() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    state.write().await.last_user_prompt = Some("what is the meaning of life?".to_string());
    let (mut input, tx) = ScriptedInput::new();

    let driver = tokio::spawn(async move {
        let mut terminal = inline_terminal();
        run(
            &mut terminal,
            state,
            client,
            test_endpoint(gateway.port),
            SessionChoice::New,
            &mut input,
        )
        .await
    });
    gateway.wait_for("commands.list", PATIENCE).await;

    tx.send(TuiAction::RunSlashCommand("/retry".to_string()))
        .expect("queued");
    let params = gateway.wait_for("chat.send", PATIENCE).await;
    assert_eq!(
        params["message"], "what is the meaning of life?",
        "the resend carries the prompt that was sent before"
    );

    tx.send(TuiAction::Quit).expect("queued");
    tokio::time::timeout(PATIENCE, driver)
        .await
        .expect("the loop exits")
        .expect("join")
        .expect("run");
}

/// `/retry` while a turn is running honors the send gate exactly like
/// Enter: the draft behind a running turn is queued, not sent in
/// parallel, and goes out when the turn's `chat.final` ends it.
#[tokio::test]
async fn retry_queues_behind_a_running_turn() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let observed = Arc::clone(&state);
    let (mut input, tx) = ScriptedInput::new();

    let driver = tokio::spawn(async move {
        let mut terminal = inline_terminal();
        run(
            &mut terminal,
            state,
            client,
            test_endpoint(gateway.port),
            SessionChoice::New,
            &mut input,
        )
        .await
    });
    gateway.wait_for("commands.list", PATIENCE).await;

    // A real send opens a turn that never ends on its own here.
    for c in "hullo".chars() {
        tx.send(TuiAction::InputChar(c)).expect("queued");
    }
    tx.send(TuiAction::SendMessage).expect("queued");
    gateway.wait_for("chat.send", PATIENCE).await;

    tx.send(TuiAction::RunSlashCommand("/retry".to_string()))
        .expect("queued");
    eventually_async(
        || {
            let observed = Arc::clone(&observed);
            async move {
                let s = observed.read().await;
                s.queued.len() == 1 && s.is_running
            }
        },
        "the retry to join the message queue behind the running turn",
    )
    .await;

    // Ending the turn releases the queued resend, and it must carry the
    // same prompt.
    gateway.push_event(
        "chat.final",
        serde_json::json!({ "session_id": "s1", "response": "answered" }),
    );
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let count = gateway
            .requests()
            .iter()
            .filter(|r| r.method == "chat.send")
            .count();
        if count >= 2 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the queued retry never went out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sends: Vec<_> = gateway
        .requests()
        .into_iter()
        .filter(|r| r.method == "chat.send")
        .collect();
    assert_eq!(
        sends[1].params["message"], "hullo",
        "the queued retry resends the prompt that was sent before"
    );

    tx.send(TuiAction::Quit).expect("queued");
    tokio::time::timeout(PATIENCE, driver)
        .await
        .expect("the loop exits")
        .expect("join")
        .expect("run");
}

/// `/retry` with nothing to resend says so; the retained prompt is not
/// invented out of thin air.
#[tokio::test]
async fn retry_without_a_prompt_refuses() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;

    retry_last_message(&state, &client).await.expect("handled");

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(lines[0].contains("nothing to retry"), "got {lines:?}");
}

/// A failed abort does not claim the run stopped.
///
/// `abort_or_quit` dropped the error and reported "── stopped ──" anyway,
/// so a lost `chat.abort` left the UI asserting a stop it had no evidence
/// for while the gateway carried on generating. The key still works: Esc
/// or Ctrl+C tries again.
#[tokio::test]
async fn a_failed_abort_does_not_claim_the_run_stopped() {
    let gateway = TestGateway::start().await;
    gateway.fail_with("chat.abort", "INTERNAL");
    let (state, client) = state_and_client(&gateway).await;
    {
        let mut s = state.write().await;
        s.current_session = Some("s1".to_string());
        s.begin_run();
    }

    abort_or_quit(&state, &client).await.expect("handled");

    let mut s = state.write().await;
    assert!(s.is_running, "the turn may still be running, and the UI says so");
    let lines: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(lines.iter().any(|l| l.contains("could not stop the run")), "got {lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("── stopped ──")),
        "and it does not claim otherwise: {lines:?}"
    );
}

/// An abort means stop, queue included.
#[tokio::test]
async fn aborting_drops_the_queue() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    {
        let mut s = state.write().await;
        s.current_session = Some("s1".to_string());
        s.begin_run();
        s.queue_message("second".to_string());
    }

    abort_or_quit(&state, &mut client).await.expect("aborted");

    let mut s = state.write().await;
    assert!(!s.is_running);
    assert!(s.queued.is_empty(), "stop means stop");
    let lines: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("queued message")),
        "and it says what it dropped: {lines:?}"
    );
}

/// The queue is not a way to lose a message quietly.
#[tokio::test]
async fn a_lost_connection_drops_the_queue_out_loud() {
    let mut s = AppState::default();
    s.begin_run();
    s.queue_message("second".to_string());
    s.connection_lost("gone");

    assert!(s.queued.is_empty());
    let lines: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(lines.iter().any(|l| l.contains("queued message")), "got {lines:?}");
}
