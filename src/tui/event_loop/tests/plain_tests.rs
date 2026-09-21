//! Line mode: the pipe, and the terminal on stdin.

use crate::tui::app::SessionChoice;
use crate::tui::event_loop::events::handle_event;
use crate::tui::event_loop::plain::run_plain_with;
use crate::tui::event_loop::prompts::{answer_prompt_from_line, answer_prompt_without_a_human};
use crate::tui::state::LiveMode;
use crate::tui::test_gateway::TestGateway;

use super::support::*;

/// A piped line is a message, not a no-op.
///
/// This is the whole point of line mode: `echo hello | syscity tui` has to
/// send `hello`. It used to read the line, check it for a leading `/` and
/// throw it away, because `send_message` submits the *input buffer* and
/// nothing ever put the line in it.
#[tokio::test]
async fn a_piped_line_is_submitted_as_a_message() {
    let gateway = TestGateway::start().await;
    let (io, _out) = plain_io("hello\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    let params = gateway.wait_for("chat.send", PATIENCE).await;
    assert_eq!(params["message"], "hello");
    assert_eq!(params["session_id"], "s1", "the created session is used");

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
}

/// Every line of a multi-line pipe is sent, in order, one turn at a time.
///
/// A pipe hands its lines over as fast as it has them, and line mode used
/// to fire every one at the session at once — the same parallel-turn bug
/// the composer had, with a script's worth of messages behind it. They
/// queue instead, and each goes out when the turn in front of it ends.
#[tokio::test]
async fn piped_lines_are_submitted_one_turn_at_a_time() {
    let gateway = TestGateway::start().await;
    let (io, _out) = plain_io("one\ntwo\nthree\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    for (idx, text) in ["one", "two", "three"].iter().enumerate() {
        eventually_async(
            || async {
                gateway
                    .requests()
                    .iter()
                    .any(|r| r.method == "chat.send" && r.params["message"].as_str() == Some(text))
            },
            &format!("a chat.send of {text:?}"),
        )
        .await;
        assert_eq!(
            gateway
                .requests()
                .iter()
                .filter(|r| r.method == "chat.send")
                .count(),
            idx + 1,
            "only the current turn has gone out"
        );
        gateway.push_event(
            "chat.final",
            serde_json::json!({ "session_id": "s1", "response": "ok\n" }),
        );
    }

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
}

/// A blank line is not a message.
#[tokio::test]
async fn blank_piped_lines_are_ignored() {
    let gateway = TestGateway::start().await;
    let (io, _out) = plain_io("\n   \nhello\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    let params = gateway.wait_for("chat.send", PATIENCE).await;
    assert_eq!(params["message"], "hello");
    assert_eq!(
        gateway
            .requests()
            .iter()
            .filter(|r| r.method == "chat.send")
            .count(),
        1,
        "only the line with content is sent"
    );

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
}

/// A streamed answer reaches the writer as it arrives, and exactly once.
///
/// Line mode used to print each `chat.delta` with `print!` *and* let
/// `chat.final` re-emit the whole turn through the transcript, so a
/// streamed response came out twice — the deltas, a blank line, then the
/// whole thing again. Routing every event through the transcript is what
/// makes "printed once" true by construction.
#[tokio::test]
async fn a_streamed_answer_is_printed_once() {
    let gateway = TestGateway::start().await;
    let (io, out) = plain_io("hello\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    gateway.wait_for("chat.send", PATIENCE).await;
    gateway.push_event(
        "chat.delta",
        serde_json::json!({ "session_id": "s1", "content": "pong from the model\n" }),
    );
    // It graduates when it arrives, not when the turn closes.
    eventually(|| out.text().contains("pong from the model"), "the streamed line").await;

    gateway.push_event(
        "chat.final",
        serde_json::json!({
            "session_id": "s1",
            "response": "pong from the model\n",
        }),
    );
    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");

    let text = out.text();
    assert_eq!(
        text.matches("pong from the model").count(),
        1,
        "printed once, not twice: {text:?}"
    );
}

/// A non-streaming turn — `chat.final` alone — still prints once: the
/// transcript's "no deltas ever arrived" path covers it.
#[tokio::test]
async fn a_non_streaming_answer_is_printed_once() {
    let gateway = TestGateway::start().await;
    let (io, out) = plain_io("hello\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    gateway.wait_for("chat.send", PATIENCE).await;
    gateway.push_event(
        "chat.final",
        serde_json::json!({ "session_id": "s1", "response": "pong from the model\n" }),
    );
    eventually(|| out.text().contains("pong from the model"), "the answer").await;

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
    let text = out.text();
    assert_eq!(text.matches("pong from the model").count(), 1, "got {text:?}");
}

/// A failing command in line mode does not eat the lines behind it.
#[tokio::test]
async fn a_failing_command_does_not_stop_the_pipe() {
    let gateway = TestGateway::start().await;
    gateway.fail_with("system.presence", "INTERNAL");
    let (io, out) = plain_io("/status\nhello\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    gateway.wait_for("chat.send", PATIENCE).await;
    eventually(|| out.text().contains("✘"), "the failure to be reported").await;
    assert_eq!(
        gateway
            .requests()
            .iter()
            .find(|r| r.method == "chat.send")
            .and_then(|r| r.params["message"].as_str()),
        Some("hello"),
        "the line after the failed command still went"
    );

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
}

/// A prompt in a pipe is denied at once, not left to time out.
///
/// Line mode's loop only ever handled `LiveMode::Composer` — the approval
/// and question branches existed in the interactive loop alone. A pipe
/// that produced one parked on the gateway's five-minute timeout with
/// every later line stuck behind it.
#[tokio::test]
async fn a_pipe_denies_an_approval_nobody_can_answer() {
    let gateway = TestGateway::start().await;
    let (io, out) = plain_io("hello\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));
    gateway.wait_for("chat.send", PATIENCE).await;

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

    let params = gateway.wait_for("approvals.deny", PATIENCE).await;
    assert_eq!(params["id"], "ap1");
    eventually(|| out.text().contains("denied file_write"), "the denial to be reported").await;

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
}

/// A question with a default is answered with it — the agent's own
/// suggestion is the least surprising answer, and it keeps the turn alive.
#[tokio::test]
async fn a_pipe_answers_a_question_with_its_default() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    handle_event(ask_event(Some("main")), &state, &mut client).await;
    answer_prompt_without_a_human(&state, &client)
        .await
        .expect("settled");

    let params = gateway.wait_for("ask.respond", PATIENCE).await;
    assert_eq!(params["response"], "main");
    let s = state.read().await;
    assert!(s.pending_ask.is_none());
    assert_eq!(s.live_mode, LiveMode::Composer);
}

/// A question with no default cannot be answered at all — `ask.respond`
/// rejects an empty response — so the turn is stopped rather than left to
/// hold the pipe for the full timeout.
#[tokio::test]
async fn a_pipe_stops_the_turn_for_a_question_it_cannot_answer() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    {
        let mut s = state.write().await;
        s.current_session = Some("s1".to_string());
        s.begin_run();
    }

    handle_event(ask_event(None), &state, &mut client).await;
    answer_prompt_without_a_human(&state, &client)
        .await
        .expect("settled");

    let params = gateway.wait_for("chat.abort", PATIENCE).await;
    assert_eq!(params["session_id"], "s1");
    assert!(
        !gateway.requests().iter().any(|r| r.method == "ask.respond"),
        "there was no answer to send"
    );
    let s = state.read().await;
    assert!(s.pending_ask.is_none());
    assert!(!s.is_running, "the turn is over");
}

/// With a terminal on stdin, `syscity tui > out.txt` can still answer.
#[tokio::test]
async fn a_typed_line_answers_an_approval() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    handle_event(approval_event(), &state, &mut client).await;
    answer_prompt_from_line("y", &state, &client)
        .await
        .expect("answered");

    let params = gateway.wait_for("approvals.approve", PATIENCE).await;
    assert_eq!(params["id"], "ap1");
    let s = state.read().await;
    assert!(s.approvals.is_empty());
    assert_eq!(s.live_mode, LiveMode::Composer);
}

/// Typing anything else does not guess at an approval.
#[tokio::test]
async fn an_unreadable_answer_leaves_the_prompt_up() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    handle_event(approval_event(), &state, &mut client).await;
    answer_prompt_from_line("maybe", &state, &client)
        .await
        .expect("handled");

    assert_eq!(state.read().await.approvals.len(), 1, "the prompt waits for a real answer");
    assert!(
        !gateway
            .requests()
            .iter()
            .any(|r| r.method == "approvals.approve" || r.method == "approvals.deny"),
        "nothing was decided"
    );
}

/// A pipe's lines are never read as prompt answers.
#[tokio::test]
async fn a_pipe_does_not_read_a_line_as_a_prompt_answer() {
    let gateway = TestGateway::start().await;
    let (io, _out) = plain_io("y\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    // `y` is a message like any other line.
    let params = gateway.wait_for("chat.send", PATIENCE).await;
    assert_eq!(params["message"], "y");

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
}

/// And a terminal on stdin is read as prompt answers, not messages.
#[tokio::test]
async fn an_interactive_line_answers_instead_of_sending() {
    let gateway = TestGateway::start().await;
    let (io, _out) = plain_io_interactive("y\n");
    let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

    // The line arrives before any prompt exists, so it is a message.
    let params = gateway.wait_for("chat.send", PATIENCE).await;
    assert_eq!(params["message"], "y");

    gateway.close().await;
    run.await.expect("join").expect("line mode exits cleanly");
}
