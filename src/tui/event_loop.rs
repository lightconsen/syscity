//! The async event loop: input, gateway events, and the two render paths.
//!
//! Each iteration does one thing — take an action, take a gateway message, or
//! tick — and then hands the screen back:
//!
//! 1. flush anything the transcript has graduated into the scrollback, then
//! 2. repaint the live region.
//!
//! That order is not optional: `insert_before` blanks the live region, so a
//! flush without a following draw leaves the composer invisible.
// INVARIANTS-NONE: event loop; state lives in `AppState`.

use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::Terminal;
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio::time::interval;

use crate::tui::actions::TuiAction;
use crate::tui::app::{Endpoint, SessionChoice};
use crate::tui::commands::handle_slash_command;
use crate::tui::error::TuiError;
use crate::tui::gateway_calls::{self as gw, ApprovalDetail};
use crate::tui::input::poll_action;
use crate::tui::resume;
use crate::tui::retry::Backoff;
use crate::tui::scrollback;
use crate::tui::state::{AppState, AskPrompt, ConnectionState, LiveMode};
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ui::{blocks, live};
use crate::tui::ws_client::{ClientEvent, WsClient, WsMessage};

/// Stream key for the turn currently being generated (one turn at a time).
const STREAM_ASSISTANT: &str = "assistant";
/// Stream key for the reasoning that accompanies it.
const STREAM_THINKING: &str = "thinking";

/// One thing the loop waits for.
enum Event {
    /// A gateway message.
    Gateway(Option<WsMessage>),
    /// The animation/expiry beat.
    Tick,
}

/// Run the TUI until the user quits or a fatal error occurs.
pub async fn run<B>(
    terminal: &mut Terminal<B>,
    state: Arc<RwLock<AppState>>,
    ws_client: WsClient,
    endpoint: Endpoint,
    session: SessionChoice,
) -> Result<(), TuiError>
where
    B: Backend,
    TuiError: From<B::Error>,
{
    let mut ws = Some(ws_client);
    let mut backoff = Backoff::new();
    let mut reconnect_at: Option<Instant> = None;
    // Short enough that typing feels immediate; the tick itself only marks the
    // state dirty when something is actually animating.
    let mut ticker = interval(Duration::from_millis(50));

    startup(&state, &mut ws, &session).await;
    redraw(terminal, &state).await?;

    loop {
        // Drain input first: the draw below issues a cursor-position query, and
        // nothing may be reading stdin while that reply is in flight.
        while let Some(action) = poll_action() {
            state.write().await.dirty = true;
            match ws.as_mut() {
                Some(client) => handle_action(action, &state, client).await?,
                None => {
                    handle_offline_action(action, &state, &mut backoff, &mut reconnect_at).await
                }
            }
            if state.read().await.should_quit {
                break;
            }
        }

        let event = tokio::select! {
            msg = next_gateway(&mut ws) => Event::Gateway(msg),
            _ = ticker.tick() => Event::Tick,
        };

        match event {
            Event::Gateway(Some(WsMessage::Disconnected)) | Event::Gateway(None) => {
                state.write().await.dirty = true;
                ws = None;
                let mut s = state.write().await;
                s.transcript
                    .push_notice("⚠ lost the gateway — reconnecting…");
                s.connection_lost("gateway went away");
                drop(s);
                schedule_reconnect(&mut backoff, &mut reconnect_at);
            }
            Event::Gateway(Some(message)) => {
                state.write().await.dirty = true;
                if let Some(client) = ws.as_mut() {
                    handle_gateway_message(message, &state, client).await;
                }
            }
            Event::Tick => {
                let changed = {
                    let mut s = state.write().await;
                    s.advance_animations()
                };
                if changed {
                    state.write().await.dirty = true;
                }
                if ws.is_none() && reconnect_at.is_some_and(|t| Instant::now() >= t) {
                    try_reconnect(&state, &mut ws, &endpoint, &mut backoff, &mut reconnect_at)
                        .await;
                }
            }
        }

        redraw(terminal, &state).await?;

        if state.read().await.should_quit {
            break;
        }
        if let Some(err) = state.read().await.fatal_error.clone() {
            return Err(TuiError::WebSocket(err));
        }
    }
    Ok(())
}

/// Await the next gateway message, or never resolve while disconnected.
async fn next_gateway(ws: &mut Option<WsClient>) -> Option<WsMessage> {
    match ws.as_mut() {
        Some(client) => client.next().await,
        None => std::future::pending().await,
    }
}

/// Everything that has to happen before the first paint.
async fn startup(
    state: &Arc<RwLock<AppState>>,
    ws: &mut Option<WsClient>,
    session: &SessionChoice,
) {
    let Some(client) = ws.as_mut() else {
        return;
    };

    // The catalog feeds `/help` and Tab completion.
    if let Ok(catalog) = gw::commands_list(client).await {
        let mut s = state.write().await;
        if !catalog.is_empty() {
            s.command_list = catalog;
        }
    }

    match resume::resolve_startup_session(session, state, client).await {
        Ok(resume::StartupSession::Use(id)) => {
            if let Err(e) = resume::switch_to(&id, state, client).await {
                state
                    .write()
                    .await
                    .transcript
                    .push_notice(format!("⚠ could not resume {id}: {e}"));
            }
        }
        Ok(resume::StartupSession::ListAndWait) => {
            // `--resume` with no id: show the list, keep `/resume <n>` as the
            // way in.
            if let Ok(sessions) = resume::refresh_sessions(state, client).await {
                let mut lines = vec![TranscriptLine::new(
                    LineKind::Notice,
                    format!("{} sessions:", sessions.len()),
                )];
                for line in resume::session_lines(&sessions) {
                    lines.push(TranscriptLine::new(LineKind::Notice, line));
                }
                lines.push(TranscriptLine::new(
                    LineKind::Notice,
                    "resume one with /resume <number>",
                ));
                state.write().await.transcript.push(lines);
            }
        }
        Ok(resume::StartupSession::Fresh) => {}
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("⚠ could not list sessions: {e}"));
        }
    }

    let greeting = {
        let s = state.read().await;
        match (&s.current_session, s.connection.label()) {
            (Some(id), label) => format!("── connected ({label}) · session {id} ──"),
            (None, label) => format!("── connected ({label}) · Ctrl+H for help ──"),
        }
    };
    state.write().await.transcript.push_notice(greeting);
}

/// Flush graduated lines, then repaint the live region.
async fn redraw<B>(
    terminal: &mut Terminal<B>,
    state: &Arc<RwLock<AppState>>,
) -> Result<(), TuiError>
where
    B: Backend,
    TuiError: From<B::Error>,
{
    let (pending, dirty) = {
        let mut s = state.write().await;
        (s.transcript.take_flushable(), s.dirty)
    };
    if pending.is_empty() && !dirty {
        return Ok(());
    }

    if !pending.is_empty() {
        let width = terminal.size()?.width;
        let lines = blocks::to_lines(&pending);
        scrollback::flush(terminal, &lines, width)?;
        // `flush` inserted above the viewport and cleared it — the draw below
        // is what puts the composer back.
    }

    {
        let s = state.read().await;
        terminal.draw(|f| live::render(f, &s))?;
    }
    state.write().await.dirty = false;
    Ok(())
}

/// Handle a key action while connected.
async fn handle_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let mode = state.read().await.live_mode;
    match mode {
        LiveMode::Approval => return handle_approval_action(action, state, ws).await,
        LiveMode::Ask => return handle_ask_action(action, state, ws).await,
        LiveMode::Composer => {}
    }

    match action {
        TuiAction::Quit => state.write().await.should_quit = true,
        TuiAction::Abort => abort_or_quit(state, ws).await?,
        TuiAction::SendMessage => send_message(state, ws).await?,
        TuiAction::RunSlashCommand(cmd) => {
            let mut s = state.write().await;
            s.transcript.push_user(&cmd);
            s.clear_input();
            drop(s);
            handle_slash_command(&cmd, Arc::clone(state), ws).await?;
        }
        TuiAction::InputChar(c) => state.write().await.insert_char(c),
        TuiAction::InputNewline => state.write().await.insert_newline(),
        TuiAction::InputBackspace => state.write().await.input_backspace(),
        TuiAction::InputDelete => {
            let mut s = state.write().await;
            s.move_cursor_right();
            s.input_backspace();
        }
        TuiAction::CursorLeft => state.write().await.move_cursor_left(),
        TuiAction::CursorRight => state.write().await.move_cursor_right(),
        TuiAction::CursorHome => state.write().await.input_cursor = 0,
        TuiAction::CursorEnd => {
            let mut s = state.write().await;
            s.input_cursor = s.input_buffer.len();
        }
        TuiAction::CursorUp => {
            state.write().await.cursor_up_or_history();
        }
        TuiAction::CursorDown => {
            state.write().await.cursor_down_or_history();
        }
        TuiAction::CompleteNext | TuiAction::CompletePrev => {
            let mut s = state.write().await;
            s.move_completion(matches!(action, TuiAction::CompleteNext));
            s.apply_completion();
        }
        TuiAction::Escape => {
            // Esc while a turn is running stops it; otherwise it clears the
            // draft, which is the least surprising "get me out of here".
            let running = state.read().await.is_running;
            if running {
                abort_or_quit(state, ws).await?;
            } else {
                state.write().await.clear_input();
            }
        }
        TuiAction::Resize(..) | TuiAction::None => {}
    }
    Ok(())
}

/// Actions that are still meaningful with the gateway gone.
async fn handle_offline_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    backoff: &mut Backoff,
    reconnect_at: &mut Option<Instant>,
) {
    let edit_only = matches!(
        action,
        TuiAction::InputChar(_)
            | TuiAction::InputNewline
            | TuiAction::InputBackspace
            | TuiAction::InputDelete
            | TuiAction::CursorLeft
            | TuiAction::CursorRight
            | TuiAction::CursorHome
            | TuiAction::CursorEnd
            | TuiAction::CursorUp
            | TuiAction::CursorDown
            | TuiAction::CompleteNext
            | TuiAction::CompletePrev
            | TuiAction::Resize(..)
            | TuiAction::None
    );
    if edit_only {
        // Editing is local; do it through the same paths as usual.
        let mut s = state.write().await;
        match action {
            TuiAction::InputChar(c) => s.insert_char(c),
            TuiAction::InputNewline => s.insert_newline(),
            TuiAction::InputBackspace => s.input_backspace(),
            TuiAction::InputDelete => {
                s.move_cursor_right();
                s.input_backspace();
            }
            TuiAction::CursorLeft => s.move_cursor_left(),
            TuiAction::CursorRight => s.move_cursor_right(),
            TuiAction::CursorHome => s.input_cursor = 0,
            TuiAction::CursorEnd => s.input_cursor = s.input_buffer.len(),
            TuiAction::CursorUp => {
                s.cursor_up_or_history();
            }
            TuiAction::CursorDown => {
                s.cursor_down_or_history();
            }
            TuiAction::CompleteNext | TuiAction::CompletePrev => {
                s.move_completion(matches!(action, TuiAction::CompleteNext));
                s.apply_completion();
            }
            _ => {}
        }
        return;
    }

    match action {
        TuiAction::Quit => state.write().await.should_quit = true,
        TuiAction::SendMessage | TuiAction::RunSlashCommand(_) => {
            // Say so rather than silently swallowing the message; the input
            // buffer is left intact so nothing is lost.
            state
                .write()
                .await
                .transcript
                .push_notice("⚠ not connected — message not sent, retry when reconnected");
            schedule_reconnect(backoff, reconnect_at);
        }
        TuiAction::Escape => {
            state.write().await.clear_input();
        }
        _ => {}
    }
}

/// Schedule the next reconnect attempt.
fn schedule_reconnect(backoff: &mut Backoff, reconnect_at: &mut Option<Instant>) {
    let delay = backoff.next_delay();
    *reconnect_at = Some(Instant::now() + delay);
}

/// Attempt to reconnect, reporting the outcome into the transcript.
async fn try_reconnect(
    state: &Arc<RwLock<AppState>>,
    ws: &mut Option<WsClient>,
    endpoint: &Endpoint,
    backoff: &mut Backoff,
    reconnect_at: &mut Option<Instant>,
) {
    *reconnect_at = None;
    let attempt = backoff.attempt() + 1;
    match WsClient::connect(&endpoint.url, &endpoint.auth, &["chat", "read", "write"]).await {
        Ok((client, hello)) => {
            *ws = Some(client);
            backoff.reset();
            let mut s = state.write().await;
            s.connection = ConnectionState::Connected {
                features: hello.features,
                scopes_granted: hello.scopes_granted,
                server_version: hello.server.version,
            };
            s.dirty = true;
            // Deliberately no history replay: the transcript is already in the
            // scrollback above, and reprinting it would duplicate everything.
            s.transcript
                .push_notice("── reconnected (output produced while offline was not received) ──");
            let session = s.current_session.clone();
            drop(s);
            if let Some(id) = session {
                if let Some(client) = ws.as_mut() {
                    let _ = gw::sessions_subscribe(client, &id).await;
                }
            }
        }
        Err(e) => {
            state
                .write()
                .await
                .set_status(format!("⚠ reconnect attempt {attempt} failed: {e}"));
            schedule_reconnect(backoff, reconnect_at);
        }
    }
}

/// Abort the running turn, or quit when idle.
async fn abort_or_quit(state: &Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let session = state.read().await.current_session.clone();
    let running = state.read().await.is_running;
    if running {
        if let Some(id) = session {
            let _ = gw::chat_abort(ws, &id).await;
        }
        let mut s = state.write().await;
        s.transcript.finish_stream(STREAM_ASSISTANT, None);
        s.transcript.finish_stream(STREAM_THINKING, None);
        s.transcript.push_notice("── stopped ──");
        s.end_run();
    } else {
        state.write().await.should_quit = true;
    }
    Ok(())
}

/// Submit whatever is in the input buffer.
async fn send_message(state: &Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let text = {
        let s = state.read().await;
        s.input_buffer.trim().to_string()
    };
    if text.is_empty() {
        return Ok(());
    }

    if text.starts_with('/') {
        let mut s = state.write().await;
        s.remember_input(&text);
        s.transcript.push_user(&text);
        s.clear_input();
        drop(s);
        return handle_slash_command(&text, Arc::clone(state), ws).await;
    }

    // A session is created lazily, on the first message — the gateway picks
    // the id, and we adopt whatever it returns.
    let session = match state.read().await.current_session.clone() {
        Some(id) => id,
        None => gw::sessions_create(ws, None).await?,
    };
    if state.read().await.current_session.is_none() {
        gw::sessions_subscribe(ws, &session).await?;
    }

    {
        let mut s = state.write().await;
        s.remember_input(&text);
        s.current_session = Some(session.clone());
        s.transcript.push_user(&text);
        s.transcript.push_separator();
        s.clear_input();
        s.begin_run();
    }

    match gw::chat_send(ws, &session, &text).await {
        Ok(result) => {
            let mut s = state.write().await;
            if !result.session_id.is_empty() && result.session_id != session {
                s.current_session = Some(result.session_id);
            }
            s.current_agent = result.agent_id.or_else(|| s.current_agent.clone());
        }
        Err(e) => {
            let mut s = state.write().await;
            s.end_run();
            s.transcript.push_notice(format!("✘ could not send: {e}"));
        }
    }
    Ok(())
}

/// Handle a key action while an approval is pending.
async fn handle_approval_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let (approve, decide) = match action {
        // `y`/Enter approve, `n`/Esc deny; arrows move the highlight.
        TuiAction::InputChar('y') | TuiAction::InputChar('Y') | TuiAction::SendMessage => {
            (true, true)
        }
        TuiAction::InputChar('n') | TuiAction::InputChar('N') | TuiAction::Escape => (false, true),
        TuiAction::CursorLeft | TuiAction::CursorRight => {
            let mut s = state.write().await;
            s.approval_approve_selected = !s.approval_approve_selected;
            (false, false)
        }
        TuiAction::Quit => {
            state.write().await.should_quit = true;
            (false, false)
        }
        // Ctrl+C gets the keyboard back. It cannot answer the approval — that
        // is the gateway's to resolve, and a tool call blocked on a human
        // stays blocked — but a prompt that swallows every key with no way out
        // is a trap, and this is the key that means "stop waiting on this"
        // everywhere else in the TUI.
        TuiAction::Abort => {
            let mut s = state.write().await;
            s.pop_approval();
            s.transcript.push_notice(
                "⚠ approval left unanswered — the tool call stays blocked on the gateway until \
                 it times out; another client can still answer it",
            );
            (false, false)
        }
        // Everything else is swallowed: while a tool is blocked on a human,
        // typing must not go into the composer.
        _ => (false, false),
    };

    let Some(approval) = state.read().await.current_approval().cloned() else {
        state.write().await.live_mode = LiveMode::Composer;
        return Ok(());
    };
    if !decide {
        return Ok(());
    }

    match gw::approvals_decide(ws, &approval.id, approve, None).await {
        Ok(()) => {
            let mut s = state.write().await;
            let mark = if approve {
                "✔ approved"
            } else {
                "✘ denied"
            };
            s.transcript.push_notice(format!(
                "{mark} {} (risk: {})",
                approval.tool_name, approval.risk_level
            ));
            s.pop_approval();
            s.dirty = true;
        }
        // `NOT_FOUND` means the approval is gone — resolved by another client,
        // expired, or the turn ended. Retrying cannot succeed, and the prompt
        // owns the keyboard, so it has to go.
        Err(TuiError::Gateway { ref code, .. }) if code == "NOT_FOUND" => {
            let mut s = state.write().await;
            s.transcript.push_notice(format!(
                "⚠ {} was already resolved — the {} was not recorded",
                approval.tool_name,
                if approve { "approval" } else { "denial" }
            ));
            s.pop_approval();
            s.dirty = true;
        }
        // Anything else may pass on a second try, and the tool call is still
        // blocked on this prompt: dropping it would leave the human with no
        // way to unblock anything.
        Err(e) => {
            state.write().await.transcript.push_notice(format!(
                "✘ could not answer the approval: {e} — y or n tries again, Ctrl+C dismisses"
            ));
        }
    }
    Ok(())
}

/// Handle a key action while the agent's question is pending.
async fn handle_ask_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let (submit, dismiss) = match action {
        TuiAction::SendMessage => (true, false),
        TuiAction::Escape => (false, true),
        TuiAction::Quit => {
            state.write().await.should_quit = true;
            (false, false)
        }
        TuiAction::InputChar(c) if c.is_ascii_digit() => {
            let mut s = state.write().await;
            let options = s.pending_ask.as_ref().map(|a| a.options.len()).unwrap_or(0);
            let idx = c.to_digit(10).unwrap_or(0) as usize;
            if idx >= 1 && idx <= options {
                s.ask_input = c.to_string();
            }
            (false, false)
        }
        TuiAction::InputChar(c) => {
            state.write().await.ask_input.push(c);
            (false, false)
        }
        TuiAction::InputBackspace => {
            state.write().await.ask_input.pop();
            (false, false)
        }
        _ => (false, false),
    };

    if dismiss {
        // Matching the web UI: the question stays pending server-side until it
        // times out, and `/answer` can still pick it up.
        let mut s = state.write().await;
        s.live_mode = LiveMode::Composer;
        s.transcript.push_notice(
            "question left unanswered — /answer <text> will still reach it (it times out in 5 minutes)",
        );
        return Ok(());
    }
    if !submit {
        return Ok(());
    }

    let (ask, typed) = {
        let s = state.read().await;
        (s.pending_ask.clone(), s.ask_input.trim().to_string())
    };
    let Some(ask) = ask else {
        state.write().await.live_mode = LiveMode::Composer;
        return Ok(());
    };
    // A digit picks the option at that position; otherwise the typed text is
    // the answer, falling back to the agent's default.
    let answer = if let Ok(idx) = typed.parse::<usize>() {
        ask.options
            .get(idx.saturating_sub(1))
            .cloned()
            .unwrap_or(typed)
    } else if !typed.is_empty() {
        typed
    } else {
        ask.default.clone().unwrap_or_default()
    };
    if answer.is_empty() {
        return Ok(());
    }

    match gw::ask_respond(ws, &ask.ask_id, &answer).await {
        Ok(()) => {
            let mut s = state.write().await;
            s.pending_ask = None;
            s.ask_input.clear();
            s.live_mode = LiveMode::Composer;
            s.transcript.push_notice(format!("answered: {answer}"));
            s.dirty = true;
        }
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("✘ could not answer: {e}"));
        }
    }
    Ok(())
}

/// Route one gateway message.
async fn handle_gateway_message(
    message: WsMessage,
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) {
    match message {
        WsMessage::Event(event) => handle_event(event, state, ws).await,
        WsMessage::OrphanResponse(response) => {
            if !response.ok {
                if let Some(err) = response.error {
                    state
                        .write()
                        .await
                        .transcript
                        .push_notice(format!("⚠ {}: {}", err.code, err.message));
                }
            }
        }
        WsMessage::Disconnected => {}
    }
}

/// Apply one server event to the state.
async fn handle_event(event: ClientEvent, state: &Arc<RwLock<AppState>>, ws: &mut WsClient) {
    let Some(payload) = event.payload else {
        return;
    };
    match event.event.as_str() {
        "chat.delta" => {
            let content = payload["content"].as_str().unwrap_or_default();
            let mut s = state.write().await;
            s.transcript
                .push_delta(STREAM_ASSISTANT, LineKind::Assistant, content);
            s.dirty = true;
        }
        "agent.thinking" => {
            let content = payload["content"].as_str().unwrap_or_default();
            let mut s = state.write().await;
            s.transcript
                .push_delta(STREAM_THINKING, LineKind::Reasoning, content);
            s.dirty = true;
        }
        "tool.calling" => {
            let tool = payload["tool_name"].as_str().unwrap_or("tool").to_string();
            // The gateway names this field `arguments`; reading `args` (as the
            // old TUI did) silently dropped every tool's parameters.
            let args = payload
                .get("arguments")
                .filter(|v| !v.is_null())
                .cloned()
                .or_else(|| payload.get("args").filter(|v| !v.is_null()).cloned());
            let mut s = state.write().await;
            s.transcript.push(tool_call_lines(&tool, args.as_ref()));
            s.dirty = true;
        }
        "tool.result" => {
            let tool = payload["tool_name"].as_str().unwrap_or("tool").to_string();
            let result = payload
                .get("result")
                .filter(|v| !v.is_null())
                .map(|v| compact(v, 6));
            let text = match result {
                Some(text) => format!("  ↳ {tool}: {text}"),
                None => format!("  ↳ {tool}: done"),
            };
            let mut s = state.write().await;
            s.transcript
                .push(vec![TranscriptLine::new(LineKind::ToolResult, text)]);
            s.dirty = true;
        }
        "chat.final" => {
            let response = payload["response"].as_str().map(str::to_string);
            let mut s = state.write().await;
            s.transcript.finish_stream(STREAM_THINKING, None);
            s.transcript
                .finish_stream(STREAM_ASSISTANT, response.as_deref());
            s.end_run();
            s.dirty = true;
        }
        "chat.error" => {
            let message = payload["message"].as_str().unwrap_or("unknown error");
            let mut s = state.write().await;
            s.transcript.finish_stream(STREAM_ASSISTANT, None);
            s.transcript
                .push_notice(format!("✘ response failed: {message}"));
            s.end_run();
            s.dirty = true;
        }
        "session.created" => {
            if let Some(id) = payload["session_id"].as_str() {
                let mut s = state.write().await;
                if s.current_session.is_none() {
                    s.current_session = Some(id.to_string());
                }
                s.dirty = true;
            }
            let _ = resume::refresh_sessions(state, ws).await;
        }
        "session.renamed" => {
            let id = payload["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let name = payload["name"].as_str().unwrap_or_default().to_string();
            let mut s = state.write().await;
            if let Some(entry) = s.sessions.iter_mut().find(|x| x.id == id) {
                entry.name = Some(name);
            }
            s.dirty = true;
        }
        "cron.completed" => {
            let name = payload["job_name"].as_str().unwrap_or("cron job");
            let status = payload["status"].as_str().unwrap_or("ok");
            let output = payload["output"].as_str().unwrap_or_default();
            let mut s = state.write().await;
            s.transcript
                .push_notice(format!("⏱ {name} [{status}] {output}"));
            s.dirty = true;
        }
        "approval.required" => {
            let id = payload["approval_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                return;
            }
            let mut detail = ApprovalDetail {
                id: id.clone(),
                tool_name: payload["tool_name"].as_str().unwrap_or("tool").to_string(),
                requested_by: payload["requested_by"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                risk_level: payload["risk_level"]
                    .as_str()
                    .unwrap_or("Medium")
                    .to_string(),
                message: payload["message"].as_str().unwrap_or_default().to_string(),
                args: None,
            };
            // The event carries no arguments, so ask for them. Without this the
            // prompt would be asking the human to approve a call they cannot
            // see. A resolved-elsewhere approval comes back NOT_FOUND.
            match gw::approvals_get(ws, &id).await {
                Ok(full) => detail = full,
                Err(TuiError::Gateway { code, .. }) if code == "NOT_FOUND" => {
                    state.write().await.transcript.push_notice(format!(
                        "⚠ approval {} was already resolved",
                        detail.tool_name
                    ));
                    return;
                }
                Err(e) => {
                    state
                        .write()
                        .await
                        .set_status(format!("⚠ no details for the approval: {e}"));
                }
            }
            let mut s = state.write().await;
            s.approvals.push_back(detail);
            s.live_mode = LiveMode::Approval;
            s.dirty = true;
        }
        "ask.required" => {
            let mut s = state.write().await;
            s.pending_ask = Some(AskPrompt {
                ask_id: payload["ask_id"].as_str().unwrap_or_default().to_string(),
                question: payload["question"].as_str().unwrap_or_default().to_string(),
                options: payload["options"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                required: payload["required"].as_bool().unwrap_or(true),
                default: payload["default"].as_str().map(str::to_string),
            });
            s.ask_input.clear();
            s.live_mode = LiveMode::Ask;
            s.dirty = true;
        }
        "ask.resolved" => {
            let mut s = state.write().await;
            s.pending_ask = None;
            s.ask_input.clear();
            if s.live_mode == LiveMode::Ask {
                s.live_mode = LiveMode::Composer;
            }
            s.dirty = true;
        }
        _ => {}
    }
}

/// Transcript lines for a tool invocation.
fn tool_call_lines(tool: &str, args: Option<&Value>) -> Vec<TranscriptLine> {
    let mut lines = vec![TranscriptLine::new(LineKind::Tool, format!("⚙ {tool}"))];
    if let Some(args) = args {
        lines.push(TranscriptLine::new(LineKind::Tool, format!("  {}", compact(args, 6))));
    }
    lines
}

/// Render a JSON value on a bounded number of lines.
fn compact(value: &Value, max_lines: usize) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    let mut lines: Vec<&str> = text.lines().take(max_lines).collect();
    if text.lines().count() > max_lines {
        lines.push("…");
    }
    lines.join("\n")
}

/// Where line mode reads its lines and writes its output.
///
/// Injected rather than hard-wired to the process's stdio so the path can be
/// driven from a test. Line mode had no coverage at all, which is how the line
/// it read came to be thrown away without anyone noticing.
pub struct PlainIo {
    /// The command lines.
    pub input: Box<dyn BufRead + Send>,
    /// Everything line mode prints.
    pub output: Box<dyn Write + Send>,
}

impl PlainIo {
    /// The real thing: the process's stdin and stdout.
    pub fn stdio() -> Self {
        Self {
            // `Stdin` is only `Read`; the buffering lives in the wrapper.
            input: Box::new(io::BufReader::new(io::stdin())),
            output: Box::new(io::stdout()),
        }
    }
}

/// Run the TUI in line mode, for a stdout that is not a terminal.
///
/// No cursor addressing and no raw mode: input is read line by line, output is
/// printed as it arrives. Enough for `echo "…" | syscity tui > out.txt`, and a
/// safe landing spot when the terminal cannot do an inline viewport.
///
/// The transcript is the only writer: a line is printed when it graduates out
/// of the transcript, so nothing is printed twice and nothing needs to know
/// which path a line came from. Streaming therefore shows up at line
/// granularity — a partial line waits for its newline — which is what a pipe
/// or a file wants anyway.
pub async fn run_plain(endpoint: Endpoint, session: SessionChoice) -> Result<(), TuiError> {
    run_plain_with(endpoint, session, PlainIo::stdio()).await
}

/// Line mode, against an injected reader and writer.
pub async fn run_plain_with(
    endpoint: Endpoint,
    session: SessionChoice,
    io: PlainIo,
) -> Result<(), TuiError> {
    let PlainIo { input, mut output } = io;
    let (mut ws, hello) =
        WsClient::connect(&endpoint.url, &endpoint.auth, &["chat", "read", "write"]).await?;

    let state = Arc::new(RwLock::new(AppState::default()));
    {
        let mut s = state.write().await;
        s.connection = ConnectionState::Connected {
            features: hello.features,
            scopes_granted: hello.scopes_granted,
            server_version: hello.server.version,
        };
        s.current_session = endpoint.session.clone();
    }

    match resume::resolve_startup_session(&session, &state, &mut ws).await {
        Ok(resume::StartupSession::Use(id)) => {
            if let Err(e) = resume::switch_to(&id, &state, &mut ws).await {
                eprintln!("could not resume {id}: {e}");
            }
        }
        Ok(resume::StartupSession::ListAndWait) => {
            if let Ok(sessions) = resume::refresh_sessions(&state, &mut ws).await {
                for line in resume::session_lines(&sessions) {
                    let _ = writeln!(output, "{line}");
                }
            }
        }
        Ok(resume::StartupSession::Fresh) => {}
        Err(e) => eprintln!("could not list sessions: {e}"),
    }
    drain(&state, output.as_mut()).await;

    // Input is read on a blocking thread: it is a blocking source and has no
    // place in the async runtime.
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::task::spawn_blocking(move || {
        let mut input = input;
        loop {
            let mut line = String::new();
            match input.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(line.trim_end().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        tokio::select! {
            Some(line) = line_rx.recv() => {
                if line.trim().is_empty() {
                    continue;
                }
                if line.starts_with('/') {
                    handle_slash_command(&line, Arc::clone(&state), &mut ws).await?;
                } else if let Err(e) = submit_plain_line(&line, &state, &mut ws).await {
                    eprintln!("send failed: {e}");
                }
                drain(&state, output.as_mut()).await;
            }
            Some(message) = ws.next() => {
                match message {
                    WsMessage::Disconnected => {
                        eprintln!("connection closed");
                        break;
                    }
                    WsMessage::Event(event) => {
                        // Every event goes through the transcript, which is
                        // then drained to the writer. Deltas that also went
                        // straight to stdout came out twice: once as they
                        // arrived, and again when `chat.final` re-stated the
                        // whole turn.
                        handle_event(event, &state, &mut ws).await;
                    }
                    WsMessage::OrphanResponse(_) => {}
                }
                drain(&state, output.as_mut()).await;
            }
        }
    }
    Ok(())
}

/// Submit one line of piped input as a chat message.
///
/// The line came from the reader, not from a composer, so it has to be put
/// into the input buffer first — [`send_message`] submits what is in that
/// buffer. Without this the line was read, checked for a leading `/`, and then
/// silently discarded along with the whole buffer.
async fn submit_plain_line(
    line: &str,
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    state.write().await.set_input(line.to_string());
    send_message(state, ws).await
}

/// Print everything the transcript has graduated.
async fn drain(state: &Arc<RwLock<AppState>>, out: &mut (dyn Write + Send)) {
    let lines = state.write().await.transcript.take_flushable();
    for line in lines {
        // A closed pipe (`| head`) is not worth reporting — the reader asked
        // us to stop — but the transcript still has to be drained.
        let _ = writeln!(out, "{}", line.text);
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::tui::test_gateway::TestGateway;

    /// How long a test waits for a frame or an effect before calling it lost.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// An endpoint pointing at a test gateway.
    fn test_endpoint(port: u16) -> Endpoint {
        let auth = crate::tui::auth::AuthConfig::None;
        let url = auth.ws_url("127.0.0.1", port, None, "tui");
        Endpoint { url, auth, session: None }
    }

    /// Poll `check` until it holds, or fail.
    async fn eventually(check: impl Fn() -> bool, what: &str) {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while tokio::time::Instant::now() < deadline {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The async form, for conditions that live behind a lock.
    async fn eventually_async<F, Fut>(mut check: F, what: &str)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while tokio::time::Instant::now() < deadline {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A terminal with an inline viewport and a real scrollback, as
    /// `tests/tui_inline.rs` builds one.
    fn inline_terminal() -> Terminal<ratatui::backend::TestBackend> {
        use ratatui::TerminalOptions;
        use ratatui::Viewport;

        let mut terminal = Terminal::with_options(
            ratatui::backend::TestBackend::new(60, 12),
            TerminalOptions {
                viewport: Viewport::Inline(live::LIVE_HEIGHT),
            },
        )
        .expect("inline terminal");
        terminal
            .set_cursor_position(ratatui::layout::Position::new(0, 0))
            .expect("cursor home");
        terminal
    }

    /// A `Write` sink the test can read back.
    #[derive(Clone, Default)]
    struct SharedOutput(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedOutput {
        fn text(&self) -> String {
            let buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&buf).into_owned()
        }
    }

    impl Write for SharedOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Line mode reading `input`, with the output captured.
    fn plain_io(input: &str) -> (PlainIo, SharedOutput) {
        let out = SharedOutput::default();
        (
            PlainIo {
                input: Box::new(Cursor::new(input.as_bytes().to_vec())),
                output: Box::new(out.clone()),
            },
            out,
        )
    }

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

    /// Every line of a multi-line pipe is sent, in order.
    #[tokio::test]
    async fn every_piped_line_is_submitted() {
        let gateway = TestGateway::start().await;
        let (io, _out) = plain_io("one\ntwo\nthree\n");
        let run = tokio::spawn(run_plain_with(test_endpoint(gateway.port), SessionChoice::New, io));

        gateway.wait_for("chat.send", PATIENCE).await;
        eventually(
            || {
                gateway
                    .requests()
                    .iter()
                    .filter(|r| r.method == "chat.send")
                    .count()
                    == 3
            },
            "three messages",
        )
        .await;
        let sent: Vec<String> = gateway
            .requests()
            .into_iter()
            .filter(|r| r.method == "chat.send")
            .map(|r| r.params["message"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(sent, vec!["one", "two", "three"]);

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
                run(&mut terminal, state, client, test_endpoint(gateway.port), SessionChoice::New)
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

    /// A pending approval, on a connection to `gateway`.
    async fn state_with_approval(gateway: &TestGateway) -> (Arc<RwLock<AppState>>, WsClient) {
        let auth = crate::tui::auth::AuthConfig::None;
        let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
        let (client, _hello) = WsClient::connect(&url, &auth, &["chat"])
            .await
            .expect("connect");

        let state = Arc::new(RwLock::new(AppState::default()));
        {
            let mut s = state.write().await;
            s.approvals.push_back(ApprovalDetail {
                id: "ap1".to_string(),
                tool_name: "file_write".to_string(),
                risk_level: "High".to_string(),
                ..Default::default()
            });
            s.live_mode = LiveMode::Approval;
        }
        (state, client)
    }

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
        let (state, mut client) = state_with_approval(&gateway).await;

        handle_approval_action(TuiAction::InputChar('y'), &state, &mut client)
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
        let (state, mut client) = state_with_approval(&gateway).await;

        handle_approval_action(TuiAction::InputChar('y'), &state, &mut client)
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
        let (state, mut client) = state_with_approval(&gateway).await;

        handle_approval_action(TuiAction::Abort, &state, &mut client)
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

    #[test]
    fn compact_trims_long_values() {
        let value = serde_json::json!({ "a": 1, "b": 2, "c": 3, "d": 4 });
        let text = compact(&value, 2);
        assert!(text.lines().count() <= 3, "got {text}");
        assert!(text.ends_with('…'));
    }

    #[test]
    fn tool_call_lines_include_the_arguments() {
        let args = serde_json::json!({ "path": "/tmp/x" });
        let lines = tool_call_lines("file_write", Some(&args));
        assert_eq!(lines[0].text, "⚙ file_write");
        assert!(lines[1].text.contains("/tmp/x"));
    }

    #[test]
    fn reconnect_schedule_grows_with_each_attempt() {
        let mut backoff = Backoff::new();
        let mut at = None;
        schedule_reconnect(&mut backoff, &mut at);
        let first = at.expect("scheduled");
        assert!(first > Instant::now());
        schedule_reconnect(&mut backoff, &mut at);
        let second = at.expect("scheduled");
        assert!(second >= first, "later attempts wait longer");
    }
}
