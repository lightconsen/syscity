//! Sending: the input buffer, the queue, and the one-turn-at-a-time gate.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::tui::commands::handle_slash_command;
use crate::tui::error::TuiError;
use crate::tui::gateway_calls as gw;
use crate::tui::state::AppState;
use crate::tui::ws_client::WsClient;

/// Submit whatever is in the input buffer.
pub(super) async fn send_message(
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
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

    // What `/retry` will resend. A slash command is never recorded: it was
    // handled locally, not sent as a message.
    state.write().await.last_user_prompt = Some(text.clone());
    submit_prompt(text, state, ws).await
}

/// Send `text` through the message path: one turn at a time, queued while a
/// run is in flight, echoed as the user's message.
///
/// Shared by Enter and `/retry`, so the two can never race the gate in
/// opposite directions.
async fn submit_prompt(
    text: String,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    if state.read().await.is_running {
        let mut s = state.write().await;
        s.remember_input(&text);
        s.transcript.push_user(&text);
        s.clear_input();
        s.queue_message(text);
        let waiting = s.queued.len();
        s.transcript.push_notice(format!(
            "⏳ queued ({waiting} waiting) — sent when this turn ends; Esc stops it and the queue"
        ));
        return Ok(());
    }

    submit_message(text, state, ws, true).await
}

/// `/retry` — resend the last prompt through the same gate as Enter.
pub(crate) async fn retry_last_message(
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let Some(text) = state.read().await.last_user_prompt.clone() else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ nothing to retry yet — send a message first");
        return Ok(());
    };
    submit_prompt(text, state, ws).await
}

/// Send `text` as a chat message, creating the session if there is none.
///
/// `echo_user` is false for a queued message: it was echoed when it was typed,
/// and printing it again when it finally goes out would read as a second
/// message.
async fn submit_message(
    text: String,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
    echo_user: bool,
) -> Result<(), TuiError> {
    // A session is created lazily, on the first message — the gateway picks
    // the id, and we adopt whatever it returns.
    let existing = { state.read().await.current_session.clone() };
    let session = match existing {
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
        if echo_user {
            s.transcript.push_user(&text);
        }
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

/// Send the oldest queued message, if there is one.
///
/// Called when a turn ends, which is the only moment the queue may move: the
/// point of queueing is that a second turn does not run alongside the first.
pub(super) async fn start_queued_message(state: &Arc<RwLock<AppState>>, ws: &WsClient) {
    loop {
        // Scoped: the guard must not be alive across the await below.
        let next = { state.write().await.pop_queued() };
        let Some(text) = next else {
            return;
        };
        match submit_message(text, state, ws, false).await {
            Ok(()) => return,
            // The queue is not a place to strand a message: say this one
            // failed, and give the rest their turn.
            Err(e) => {
                let mut s = state.write().await;
                s.end_run();
                s.transcript
                    .push_notice(format!("✘ could not send a queued message: {e}"));
            }
        }
    }
}
