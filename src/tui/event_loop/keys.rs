//! Key actions: what a keystroke does with a gateway, and without one.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use crate::tui::actions::TuiAction;
use crate::tui::commands::handle_slash_command;
use crate::tui::error::TuiError;
use crate::tui::gateway_calls as gw;
use crate::tui::retry::Backoff;
use crate::tui::state::{AppState, LiveMode};
use crate::tui::ws_client::WsClient;

use super::prompts::{handle_approval_action, handle_ask_action};
use super::reconnect::schedule_reconnect;
use super::send::send_message;
use super::{STREAM_ASSISTANT, STREAM_THINKING};

/// Handle a key action while connected.
pub(super) async fn handle_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
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
        TuiAction::CopyAnswer => crate::tui::commands::command_copy(Arc::clone(state)).await,
        TuiAction::SendMessage => send_message(state, ws).await?,
        TuiAction::RunSlashCommand(cmd) => {
            let mut s = state.write().await;
            s.transcript.push_user(&cmd);
            s.clear_input();
            drop(s);
            handle_slash_command(&cmd, Arc::clone(state), ws).await?;
        }
        TuiAction::InputChar(c) => state.write().await.insert_char(c),
        TuiAction::Paste(text) => state.write().await.insert_paste(&text),
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
pub(super) async fn handle_offline_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    backoff: &mut Backoff,
    reconnect_at: &mut Option<Instant>,
) {
    let edit_only = matches!(
        action,
        TuiAction::InputChar(_)
            | TuiAction::Paste(_)
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
            TuiAction::Paste(text) => s.insert_paste(&text),
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
        TuiAction::CopyAnswer => crate::tui::commands::command_copy(Arc::clone(state)).await,
        _ => {}
    }
}

/// Abort the running turn, or quit when idle.
pub(super) async fn abort_or_quit(
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let session = state.read().await.current_session.clone();
    let running = state.read().await.is_running;
    if !running {
        state.write().await.should_quit = true;
        return Ok(());
    }

    // The gateway has to hear about it. `chat.abort` only fails for transport
    // reasons — the handler itself always reports "aborted" — so a failure
    // means we do not know whether the turn is still running. Claiming
    // "stopped" anyway would leave the UI asserting something we have no
    // evidence for, while the gateway keeps generating into a transcript
    // nobody is watching.
    if let Some(id) = session {
        if let Err(e) = gw::chat_abort(ws, &id).await {
            state.write().await.transcript.push_notice(format!(
                "✘ could not stop the run: {e} — it may still be going; press Esc or Ctrl+C again"
            ));
            return Ok(());
        }
    }
    // No session means no turn can be running on the gateway, so there is
    // nothing to tell it about.

    let mut s = state.write().await;
    s.transcript.finish_stream(STREAM_ASSISTANT, None);
    s.transcript.finish_stream(STREAM_THINKING, None);
    s.transcript.push_notice("── stopped ──");
    s.end_run();
    // "Stop" means stop: the queued messages were lined up behind the turn
    // that was just cancelled, and sending them now would be the opposite of
    // what the key asked for.
    let dropped = s.clear_queue();
    if dropped > 0 {
        s.transcript
            .push_notice(format!("⚠ {dropped} queued message(s) dropped"));
    }
    Ok(())
}
