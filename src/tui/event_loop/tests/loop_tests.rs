//! The loop itself: dispatch rules, the running loop, and the render paths.

use std::io;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::tui::actions::TuiAction;
use crate::tui::app::SessionChoice;
use crate::tui::error::TuiError;
use crate::tui::event_loop::keys::handle_action;
use crate::tui::event_loop::{absorb_action_error, dispatch, run, Dispatch, STREAM_ASSISTANT};
use crate::tui::gateway_calls::ApprovalDetail;
use crate::tui::state::{AppState, ConnectionState, LiveMode};
use crate::tui::test_gateway::TestGateway;
use crate::tui::transcript::LineKind;
use crate::tui::ws_client::WsClient;

use super::support::*;

/// The rule the loop dispatches on.
#[test]
fn one_command_at_a_time_edits_never_queue_and_quit_waits_for_nothing() {
    use TuiAction::{InputChar, Quit, RunSlashCommand, SendMessage};

    // `/retry` is a command: it answers to the send gate exactly like a
    // typed message — queued behind a busy turn, run when idle.
    assert_eq!(dispatch(&RunSlashCommand("/retry".to_string()), true, false), Dispatch::Run);
    assert_eq!(dispatch(&RunSlashCommand("/retry".to_string()), true, true), Dispatch::Queue);

    // Idle and online: run it.
    assert_eq!(dispatch(&SendMessage, true, false), Dispatch::Run);
    // A command is already in flight: it waits its turn instead of racing
    // it — two `/new`s finishing in the wrong order would leave the state
    // describing a session the user is not in.
    assert_eq!(dispatch(&SendMessage, true, true), Dispatch::Queue);
    // An edit never waits: typing during a slow request is the point of
    // the loop being non-blocking.
    assert_eq!(dispatch(&InputChar('x'), true, true), Dispatch::Edit);
    assert_eq!(dispatch(&InputChar('x'), true, false), Dispatch::Edit);
    // No gateway: the offline path still lets the user edit and says the
    // message was not sent, rather than queueing it forever.
    assert_eq!(dispatch(&SendMessage, false, false), Dispatch::Offline);
    assert_eq!(dispatch(&SendMessage, false, true), Dispatch::Offline);
    assert_eq!(dispatch(&InputChar('x'), false, false), Dispatch::Offline);
    // Quit never waits — not for a gateway, not for a running command.
    assert_eq!(dispatch(&Quit, true, false), Dispatch::Quit);
    assert_eq!(dispatch(&Quit, true, true), Dispatch::Quit);
    assert_eq!(dispatch(&Quit, false, true), Dispatch::Quit);
}

/// The loop does not wait for a request that never comes back.
///
/// Startup ran as a prologue — `startup(...).await` ahead of the loop and
/// of the first paint — so a gateway that did not answer meant eight blank
/// rows, no input, and no events, for as long as it took. It is the first
/// piece of work now, and the loop is running underneath it.
#[tokio::test]
async fn the_loop_runs_while_a_request_is_still_in_flight() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;

    // Park startup on its first request, and never answer it.
    gateway.hold("commands.list");

    let driver = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            let mut terminal = inline_terminal();
            run(
                &mut terminal,
                state,
                client,
                test_endpoint(gateway.port),
                SessionChoice::New,
                &mut SilentInput,
            )
            .await
        }
    });

    // Startup really did start, and is really still waiting.
    gateway.wait_for("commands.list", PATIENCE).await;

    // The loop is alive underneath it, so it sees this and stops.
    state.write().await.should_quit = true;
    let finished = tokio::time::timeout(PATIENCE, driver).await;
    assert!(finished.is_ok(), "the loop waited for a request the gateway never answered");
    finished
        .expect("checked")
        .expect("join")
        .expect("the loop exits");
}

/// Losing the connection converges the loop's state, not just the client's.
///
/// Driver the real [`run`] against the test gateway: a run in progress and
/// a pending approval, then the socket goes away. Neither can make
/// progress without a gateway, and a pending approval owns the keyboard —
/// left set, it swallows every keystroke and the composer is unreachable.
#[tokio::test]
async fn a_lost_connection_converges_the_running_loop() {
    let gateway = TestGateway::start().await;
    let state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut s = state.write().await;
        s.begin_run();
        s.approvals.push_back(ApprovalDetail {
            id: "ap1".to_string(),
            tool_name: "file_write".to_string(),
            ..Default::default()
        });
        s.live_mode = LiveMode::Approval;
        s.transcript
            .push_delta(STREAM_ASSISTANT, LineKind::Assistant, "half an answer");
    }

    let auth = crate::tui::auth::AuthConfig::None;
    let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
    let (client, _hello) = WsClient::connect(&url, &auth, &["chat"])
        .await
        .expect("connect");

    let driver = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            let mut terminal = inline_terminal();
            run(
                &mut terminal,
                state,
                client,
                test_endpoint(gateway.port),
                SessionChoice::New,
                &mut SilentInput,
            )
            .await
        }
    });

    gateway.close().await;
    eventually_async(
        || {
            let state = Arc::clone(&state);
            async move { !state.read().await.is_running }
        },
        "the run to end",
    )
    .await;

    {
        let s = state.read().await;
        assert!(matches!(s.connection, ConnectionState::Lost(_)));
        assert!(s.approvals.is_empty(), "a prompt that cannot be answered is not kept");
        assert_eq!(s.live_mode, LiveMode::Composer, "the composer takes the input back");
        assert!(s.pending_ask.is_none());
        assert!(
            s.transcript.preview(10).is_empty(),
            "nothing is left hanging in the live region"
        );
    }

    state.write().await.should_quit = true;
    driver.await.expect("join").expect("the loop exits");
}

/// A command that fails is a notice, not the end of the TUI.
///
/// `handle_action` propagated every error, so a timed-out `/status` — or
/// any other command the gateway refused — tore the whole TUI down while
/// an ordinary chat error only printed a line.
#[tokio::test]
async fn a_failing_command_does_not_end_the_session() {
    let gateway = TestGateway::start().await;
    gateway.fail_with("system.presence", "INTERNAL");
    let (state, client) = state_and_client(&gateway).await;

    let err = handle_action(TuiAction::RunSlashCommand("/status".to_string()), &state, &client)
        .await
        .expect_err("the command itself failed");
    assert!(!err.is_fatal(), "and the failure is survivable: {err:?}");

    absorb_action_error(err, &state)
        .await
        .expect("the session carries on");

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(
        lines.iter().any(|l| l.starts_with("✘")),
        "the failure is a line of output: {lines:?}"
    );
}

/// An un-drawable terminal still ends the session — it has to, there is
/// nothing left to draw to.
#[tokio::test]
async fn a_broken_terminal_still_ends_the_session() {
    let state = Arc::new(RwLock::new(AppState::default()));
    let err = absorb_action_error(TuiError::Terminal(io::Error::other("tty gone")), &state)
        .await
        .expect_err("nothing to carry on with");
    assert!(err.is_fatal());
}

/// A window resize repaints the live region at the new size.
///
/// The loop treats `Resize` as a dirty-mark like any other action, and the
/// next draw runs ratatui's inline-viewport re-layout. Until the input
/// seam existed there was no way to put a resize in front of the real
/// loop — the audit's acceptance line ("no overlap, no truncation, no
/// scrollback damage") had no evidence at all.
#[tokio::test]
async fn a_resize_repaints_the_live_region_at_the_new_size() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let (mut input, tx) = ScriptedInput::new();
    let mut terminal = inline_terminal(); // 60x12

    // The terminal really changed size — crossterm would have reported it.
    terminal.backend_mut().resize(40, 6);
    tx.send(TuiAction::Resize(40, 6)).expect("queued");
    tx.send(TuiAction::Quit).expect("queued");

    run(
        &mut terminal,
        state,
        client,
        test_endpoint(gateway.port),
        SessionChoice::New,
        &mut input,
    )
    .await
    .expect("the loop exits on Quit");

    // The final draw used the new size: the viewport re-clamped to the
    // shorter terminal and the composer is alive in it.
    let area = terminal.backend().buffer().area;
    assert_eq!(area.width, 40, "repainted at the new width");
    assert!(area.height <= 6, "the inline viewport clamps to the height");
    let screen = painted(&terminal);
    assert!(screen.contains("> "), "the composer survived the resize: {screen:?}");
    let cursor = terminal.get_cursor_position().expect("a cursor");
    assert!(
        cursor.x < 40 && cursor.y <= 6,
        "the cursor sits inside the new frame: {cursor:?}"
    );
}

/// Keys reach the composer while a request is still on the wire.
///
/// This is the audit's TUI-007 acceptance line — "input still editable
/// while the gateway takes 15 seconds" — asserted end to end: startup is
/// parked on a request the gateway never answers, and a keystroke must
/// still land.
#[tokio::test]
async fn typing_lands_while_a_request_is_still_on_the_wire() {
    let gateway = TestGateway::start().await;
    gateway.hold("commands.list"); // startup parks here, forever
    let (state, client) = state_and_client(&gateway).await;
    let observed = Arc::clone(&state); // the test's window on the loop's state
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

    // The request is genuinely unanswered: startup is waiting on it.
    gateway.wait_for("commands.list", PATIENCE).await;

    tx.send(TuiAction::InputChar('h')).expect("queued");
    tx.send(TuiAction::InputChar('i')).expect("queued");
    eventually_async(
        || {
            let observed = Arc::clone(&observed);
            async move { observed.read().await.input_buffer == "hi" }
        },
        "keystrokes to land while the RPC is still in flight",
    )
    .await;

    tx.send(TuiAction::Quit).expect("queued");
    tokio::time::timeout(PATIENCE, driver)
        .await
        .expect("the loop exits despite the unanswered request")
        .expect("join")
        .expect("run");
}

/// A bracketed paste lands in the composer as content, newlines included.
///
/// Pasted text must not be re-read as keystrokes — in particular the `\n`
/// inside it is content, not an Enter. Only the Enter key sends.
#[tokio::test]
async fn a_pasted_newline_is_content_not_a_send() {
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

    tx.send(TuiAction::Paste("hi\n there".into()))
        .expect("queued");
    eventually_async(
        || {
            let observed = Arc::clone(&observed);
            async move { observed.read().await.input_buffer == "hi\n there" }
        },
        "the paste to reach the composer whole",
    )
    .await;
    // The newline inside the paste was content: nothing hit the wire.
    assert!(
        !gateway.requests().iter().any(|r| r.method == "chat.send"),
        "a paste containing \\n must not send"
    );

    tx.send(TuiAction::Quit).expect("queued");
    tokio::time::timeout(PATIENCE, driver)
        .await
        .expect("the loop exits")
        .expect("join")
        .expect("run");
}
