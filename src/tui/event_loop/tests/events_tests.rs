//! Routing gateway events into the state.

use std::sync::Arc;
use std::time::Duration;

use crate::tui::actions::TuiAction;
use crate::tui::app::SessionChoice;
use crate::tui::event_loop::events::{handle_event, tool_call_lines};
use crate::tui::event_loop::run;
use crate::tui::state::{LiveMode, RunPhase};
use crate::tui::test_gateway::TestGateway;
use crate::tui::ui::blocks;

use super::support::*;

/// An approval raised in *another* session is a notice here, not a
/// prompt: the gateway routes approvals by session now, and in the
/// fail-open window (no subscriptions yet) a stray one must neither steal
/// the keyboard nor vanish — someone's turn is blocked on it.
#[tokio::test]
async fn an_approval_for_another_session_is_a_notice_not_a_prompt() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    handle_event(
        event(
            "approval.required",
            serde_json::json!({
                "approval_id": "ap9",
                "tool_name": "file_write",
                "session_id": "s2",
            }),
        ),
        &state,
        &mut client,
    )
    .await;

    let mut s = state.write().await;
    assert!(s.approvals.is_empty(), "no prompt for another session's approval");
    assert_eq!(s.live_mode, LiveMode::Composer, "and the keyboard stays ours");
    let lines: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("another session")),
        "the blocked turn is announced: {lines:?}"
    );
}

/// The same event naming *our* session still opens the prompt.
#[tokio::test]
async fn an_approval_for_our_session_opens_the_prompt() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    handle_event(
        event(
            "approval.required",
            serde_json::json!({
                "approval_id": "ap1",
                "tool_name": "file_write",
                "session_id": "s1",
            }),
        ),
        &state,
        &mut client,
    )
    .await;

    let s = state.read().await;
    assert_eq!(s.approvals.len(), 1, "our approval prompts");
    assert_eq!(s.live_mode, LiveMode::Approval);
}

/// A slash command whose request never comes back ends as a notice, not
/// as a dead TUI.
///
/// The request bound is what keeps a wedged gateway from holding the
/// command's place in the queue forever; when it fires, the error is a
/// line of output and the loop carries on. The timeout is shortened here
/// — waiting the production 15 seconds would make this a very slow test.
#[tokio::test]
async fn a_command_that_times_out_becomes_a_notice() {
    let gateway = TestGateway::start().await;
    gateway.hold("commands.list");
    let (state, client) = state_and_client(&gateway).await;
    let client = client.with_request_timeout(Duration::from_millis(250));
    let observed = Arc::clone(&state);
    let (mut input, tx) = ScriptedInput::new();

    // The task hands the terminal back, because the notice is a *notice*:
    // it goes to the transcript, which the loop drains into scrollback
    // every iteration. Reading it means reading what was painted.
    let driver = tokio::spawn(async move {
        let mut terminal = inline_terminal();
        let result = run(
            &mut terminal,
            state,
            client,
            test_endpoint(gateway.port),
            SessionChoice::New,
            &mut input,
        )
        .await;
        (result, terminal)
    });

    tx.send(TuiAction::RunSlashCommand("/tools".to_string()))
        .expect("queued");
    // The command is stuck on the gateway; it becomes a notice when its
    // own bound fires. Quitting before that would end the loop with the
    // command still parked, so wait out the bound (250ms) with margin —
    // and nothing else here would end the loop, which is the point.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!driver.is_finished(), "a held command must not end the session");
    let _ = observed; // the loop's window on state, kept for symmetry
    tx.send(TuiAction::Quit).expect("queued");
    let (result, terminal) = tokio::time::timeout(Duration::from_secs(10), driver)
        .await
        .expect("the loop is still running")
        .expect("join");
    result.expect("clean exit");

    let backend = terminal.backend();
    let mut painted: String = backend
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    painted.extend(backend.scrollback().content.iter().map(|c| c.symbol()));
    assert!(
        painted.contains("timed out waiting for `commands.list`"),
        "the timeout is a line of output: {painted:?}"
    );
}

/// Reasoning gets a header once per turn, and a reasoning paragraph that
/// never saw a newline freezes *before* the answer it produced — not after
/// it at `chat.final`.
#[tokio::test]
async fn reasoning_gets_a_header_and_seals_before_the_answer() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    // One long reasoning line, no newline: the shape that used to be held
    // until the turn closed.
    handle_event(
            event(
                "agent.thinking",
                serde_json::json!({ "session_id": "s1", "content": "用户想知道重启时间，让我用 shell 查" }),
            ),
            &state,
            &mut client,
        )
        .await;
    handle_event(
        event(
            "chat.delta",
            serde_json::json!({ "session_id": "s1", "content": "重启时间是今天凌晨。\n" }),
        ),
        &state,
        &mut client,
    )
    .await;

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(
        lines,
        vec![
            "thinking:".to_string(),
            "用户想知道重启时间，让我用 shell 查".to_string(),
            "重启时间是今天凌晨。".to_string(),
        ],
        "header, then the reasoning, then the answer"
    );
}

/// A second reasoning burst in the same turn (the agentic loop's next
/// round) does not get a second header — the stream is still open.
#[tokio::test]
async fn a_second_reasoning_burst_gets_no_second_header() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    for content in ["first thought\n", "second thought\n"] {
        handle_event(
            event("agent.thinking", serde_json::json!({ "session_id": "s1", "content": content })),
            &state,
            &mut client,
        )
        .await;
    }

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(
        lines.iter().filter(|l| l.as_str() == "thinking:").count(),
        1,
        "one header for the turn: {lines:?}"
    );
}

/// The phase hint tracks what is actually arriving.
#[tokio::test]
async fn the_run_phase_follows_the_events() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    for (name, payload, expected) in [
        (
            "agent.thinking",
            serde_json::json!({ "session_id": "s1", "content": "hm" }),
            RunPhase::Thinking,
        ),
        (
            "chat.delta",
            serde_json::json!({ "session_id": "s1", "content": "hi" }),
            RunPhase::Responding,
        ),
        (
            "tool.calling",
            serde_json::json!({ "session_id": "s1", "tool_name": "file_read" }),
            RunPhase::ToolCall("file_read".to_string()),
        ),
        // The call finished; the wait that follows is not labelled with it.
        (
            "tool.result",
            serde_json::json!({ "session_id": "s1", "tool_name": "file_read" }),
            RunPhase::Waiting,
        ),
    ] {
        handle_event(event(name, payload), &state, &mut client).await;
        assert_eq!(state.read().await.run_phase, expected, "after {name}");
    }
}

/// Another session's traffic never reaches this transcript.
///
/// A connection with no subscriptions receives every session's events —
/// the gateway reads an empty subscription list as "all" — and a session
/// created from here is unsubscribed until its id comes back. In that
/// window, and whenever the gateway's fail-open applies, another
/// conversation's answer would otherwise be printed as ours.
#[tokio::test]
async fn events_for_another_session_are_ignored() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("mine".to_string());

    handle_event(
        event(
            "chat.delta",
            serde_json::json!({ "session_id": "theirs", "content": "not ours\n" }),
        ),
        &state,
        &mut client,
    )
    .await;
    assert!(
        state.read().await.transcript.preview(10).is_empty(),
        "another session's delta is dropped"
    );

    handle_event(
        event("chat.delta", serde_json::json!({ "session_id": "mine", "content": "ours\n" })),
        &state,
        &mut client,
    )
    .await;
    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert_eq!(lines, vec!["ours"]);
}

/// Before there is a session, no session's event is ours.
#[tokio::test]
async fn session_scoped_events_are_dropped_before_a_session_exists() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;

    handle_event(
        event(
            "chat.final",
            serde_json::json!({ "session_id": "someone-elses", "response": "hello\n" }),
        ),
        &state,
        &mut client,
    )
    .await;

    let s = state.read().await;
    assert!(s.current_session.is_none(), "and it does not get adopted");
    assert!(s.transcript.preview(10).is_empty());
}

/// Delegation updates build a live row, update it in place, and graduate
/// it to a transcript notice on terminal status.
#[tokio::test]
async fn delegation_updates_build_and_retire_rows() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    let running = serde_json::json!({
        "session_id": "s1",
        "task_id": "run-1",
        "root_id": "root-1",
        "agent_id": "researcher",
        "title": "scan docs",
        "status": "running",
        "usage_tokens": 0,
    });
    handle_event(event("delegation.updated", running.clone()), &state, &mut client).await;
    {
        let s = state.read().await;
        assert_eq!(s.delegation_tasks.len(), 1);
        assert_eq!(s.delegation_tasks[0].task_id, "run-1");
        assert!(s.delegation_tasks[0].started.is_some(), "the local clock started");
    }

    // A usage bump updates the row in place instead of stacking a second.
    let mut bump = running.clone();
    bump["usage_tokens"] = serde_json::json!(3400);
    handle_event(event("delegation.updated", bump), &state, &mut client).await;
    {
        let s = state.read().await;
        assert_eq!(s.delegation_tasks.len(), 1, "the slot is reused");
        assert_eq!(s.delegation_tasks[0].usage_tokens, 3400);
        assert!(s.delegation_tasks[0].started.is_some(), "the clock survives the update");
    }

    // Terminal status graduates the row to a notice and retires it.
    let done = serde_json::json!({
        "session_id": "s1",
        "task_id": "run-1",
        "root_id": "root-1",
        "agent_id": "researcher",
        "title": "scan docs",
        "status": "completed",
        "usage_tokens": 5100,
        "duration_ms": 188_000,
    });
    handle_event(event("delegation.updated", done), &state, &mut client).await;

    let mut s = state.write().await;
    assert!(s.delegation_tasks.is_empty(), "the terminal row is retired");
    let notices: Vec<String> = s
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        notices
            .iter()
            .any(|l| l.contains("✓ researcher: scan docs") && l.contains("5.1k tokens")),
        "the notice carries the outcome: {notices:?}"
    );
}

/// Another session's delegation update never reaches this task board.
#[tokio::test]
async fn delegation_updates_for_another_session_are_dropped() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("mine".to_string());

    handle_event(
        event(
            "delegation.updated",
            serde_json::json!({
                "session_id": "theirs",
                "task_id": "run-9",
                "root_id": "r",
                "agent_id": "worker",
                "title": "theirs",
                "status": "running",
                "usage_tokens": 0,
            }),
        ),
        &state,
        &mut client,
    )
    .await;
    assert!(
        state.read().await.delegation_tasks.is_empty(),
        "another session's task is dropped"
    );
}

/// Per-round usage accumulates into the run counter and clears with the
/// run. `chat.final` also carries the turn's usage; counting both would
/// double the total, which is why the counter only ever reads the
/// per-round events.
#[tokio::test]
async fn agent_usage_accumulates_and_clears_with_the_run() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());
    state.write().await.begin_run();

    for total in [1_200u64, 1_800] {
        handle_event(
                event(
                    "agent.usage",
                    serde_json::json!({ "session_id": "s1", "agent_id": "a", "usage": { "total_tokens": total } }),
                ),
                &state,
                &mut client,
            )
            .await;
    }
    assert_eq!(state.read().await.run_tokens, Some(3_000));

    handle_event(
        event("chat.final", serde_json::json!({ "session_id": "s1", "response": "done" })),
        &state,
        &mut client,
    )
    .await;
    assert_eq!(state.read().await.run_tokens, None, "the run ended; the counter with it");
}

/// A session list that could not be refreshed says so.
///
/// `session.created` refreshed it with `let _ =`, so a failure left
/// `/resume`, `/sessions` and the status row reading from a list that had
/// quietly stopped matching the gateway — indistinguishable from one that
/// simply had not changed.
#[tokio::test]
async fn a_failed_session_refresh_is_reported() {
    let gateway = TestGateway::start().await;
    gateway.fail_with("sessions.list", "INTERNAL");
    let (state, client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("s1".to_string());

    handle_event(
        event("session.created", serde_json::json!({ "session_id": "s1" })),
        &state,
        &client,
    )
    .await;

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("could not refresh the session list")),
        "got {lines:?}"
    );
}

/// Events that name no session are global, and still arrive.
#[tokio::test]
async fn events_without_a_session_still_arrive() {
    let gateway = TestGateway::start().await;
    let (state, mut client) = state_and_client(&gateway).await;
    state.write().await.current_session = Some("mine".to_string());

    handle_event(
        event(
            "cron.completed",
            serde_json::json!({ "job_name": "nightly", "status": "ok", "output": "done" }),
        ),
        &state,
        &mut client,
    )
    .await;

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("nightly")),
        "a global notice is not filtered: {lines:?}"
    );
}

/// A multi-line tool call must arrive as one transcript line per row:
/// the cell renderer drops a `\n` inside a line (zero width), so a
/// packed value renders as one run-together row.
#[test]
fn live_tool_args_are_one_line_per_row() {
    let args = serde_json::json!({
        "alpha": 1, "beta": 2, "gamma": 3, "delta": 4, "epsilon": 5, "zeta": 6
    });
    let lines = tool_call_lines("shell", Some(&args));
    for line in &lines {
        assert!(!line.text.contains('\n'), "packed into one row: {:?}", line.text);
    }
}

#[test]
fn tool_call_lines_include_the_arguments() {
    let args = serde_json::json!({ "path": "/tmp/x" });
    let lines = tool_call_lines("file_write", Some(&args));
    assert_eq!(lines[0].text, "⚙ file_write");
    assert!(
        lines.iter().any(|l| l.text.contains("/tmp/x")),
        "the argument is somewhere in the rows: {lines:?}"
    );
}

/// The live preview must never exceed what the region can hold, and must
/// never starve the text being written into the same space.
#[test]
fn live_tool_args_are_capped_to_the_preview() {
    let big: Vec<usize> = (0..50).collect();
    let lines = tool_call_lines("shell", Some(&serde_json::json!({ "items": big })));
    assert!(
        lines.len() <= 1 + blocks::TOOL_ARG_LINES_LIVE,
        "header plus the cap: {} lines",
        lines.len()
    );
    assert!(lines.last().expect("rows").text.trim_end().ends_with('…'));
}
