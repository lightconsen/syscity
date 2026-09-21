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

mod events;
mod keys;
mod plain;
mod prompts;
mod reconnect;
mod send;
mod startup;

#[cfg(test)]
mod tests;

pub use plain::run_plain;
pub(crate) use send::retry_last_message;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::interval;

use crate::tui::actions::TuiAction;
use crate::tui::app::{Endpoint, SessionChoice};
use crate::tui::error::TuiError;
use crate::tui::input::InputSource;
use crate::tui::retry::Backoff;
use crate::tui::scrollback;
use crate::tui::state::AppState;
use crate::tui::ui::{blocks, live};
use crate::tui::ws_client::{WsClient, WsMessage};

use events::handle_gateway_message;
use keys::{handle_action, handle_offline_action};
use reconnect::{schedule_reconnect, try_reconnect};
use startup::startup;

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
    /// The command that was running has finished.
    Finished(Result<(), TuiError>),
    /// The process was asked to terminate.
    Signal,
}

/// Resolve when the OS asks the process to go away.
///
/// Ctrl+C is *not* this path while the TUI runs: crossterm's raw mode clears
/// ISIG, so the terminal turns it into a `0x03` key event and the loop's
/// Abort handling answers. What remains is everything that does come through
/// as a signal: `kill`, a supervisor stopping us, SIGHUP from a detached
/// SSH session. With no handler those die here with the default action —
/// mid-raw-mode, leaving the user a terminal that echoes nothing. Mapping
/// them to the Quit flag ends the loop through the ordinary path, which
/// restores the terminal.
///
/// Registration happens *before* the first draw, not lazily: the frame is up
/// and the composer already looks killable while the loop spins its first
/// select, so a signal landing there must be caught too. Registering a kind
/// twice in one process fails (unit tests run several loops); a failed arm
/// degrades to never-fires, which is the pre-existing behaviour, not worse.
pub struct SignalWatch {
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hangup: Option<tokio::signal::unix::Signal>,
}

impl SignalWatch {
    #[cfg(unix)]
    fn new() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        Self {
            interrupt: signal(SignalKind::interrupt()).ok(),
            terminate: signal(SignalKind::terminate()).ok(),
            hangup: signal(SignalKind::hangup()).ok(),
        }
    }

    #[cfg(not(unix))]
    fn new() -> Self {
        Self {}
    }

    /// Resolve on the first signal that arrives — or never.
    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = recv_opt(&mut self.interrupt) => {}
                _ = recv_opt(&mut self.terminate) => {}
                _ = recv_opt(&mut self.hangup) => {}
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await
    }
}

#[cfg(unix)]
async fn recv_opt(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Run the TUI until the user quits or a fatal error occurs.
pub async fn run<B>(
    terminal: &mut Terminal<B>,
    state: Arc<RwLock<AppState>>,
    ws_client: WsClient,
    endpoint: Endpoint,
    session: SessionChoice,
    input: &mut dyn InputSource,
) -> Result<(), TuiError>
where
    B: Backend,
    TuiError: From<B::Error>,
{
    let mut ws: Option<Arc<WsClient>> = Some(Arc::new(ws_client));
    let mut backoff = Backoff::new();
    let mut reconnect_at: Option<Instant> = None;
    let mut signals = SignalWatch::new();
    // Short enough that typing feels immediate; the tick itself only marks the
    // state dirty when something is actually animating.
    let mut ticker = interval(Duration::from_millis(50));

    // Commands run as tasks, so the loop keeps polling input and painting while
    // they wait on the gateway. One at a time, the rest in order behind it: two
    // `/new`s racing would leave the state describing whichever finished last.
    let mut in_flight: Option<JoinHandle<Result<(), TuiError>>> = None;
    let mut queued: VecDeque<TuiAction> = VecDeque::new();

    // Startup is the first piece of work rather than a prologue. It makes two
    // requests, and painting the frame before them is the difference between a
    // usable composer and eight blank rows when the gateway is slow to answer.
    if let Some(client) = ws.as_ref() {
        let state = Arc::clone(&state);
        let client = Arc::clone(client);
        let session = session.clone();
        in_flight = Some(tokio::spawn(async move {
            startup(&state, &client, &session).await;
            Ok(())
        }));
    }
    redraw(terminal, &state).await?;

    loop {
        // Drain input first: the draw below issues a cursor-position query, and
        // nothing may be reading stdin while that reply is in flight.
        while let Some(action) = input.poll() {
            state.write().await.dirty = true;
            match dispatch(&action, ws.is_some(), in_flight.is_some()) {
                Dispatch::Quit => state.write().await.should_quit = true,
                Dispatch::Offline => {
                    handle_offline_action(action, &state, &mut backoff, &mut reconnect_at).await
                }
                Dispatch::Edit => {
                    if let Some(client) = ws.as_ref() {
                        if let Err(e) = handle_action(action, &state, client).await {
                            absorb_action_error(e, &state).await?;
                        }
                    }
                }
                Dispatch::Queue => queued.push_back(action),
                Dispatch::Run => {
                    if let Some(client) = ws.as_ref() {
                        in_flight = Some(spawn_action(action, &state, Arc::clone(client)));
                    }
                }
            }
            if state.read().await.should_quit {
                break;
            }
        }

        let event = tokio::select! {
            msg = next_gateway(&ws) => Event::Gateway(msg),
            _ = ticker.tick() => Event::Tick,
            done = settle(&mut in_flight) => Event::Finished(done),
            () = signals.recv() => Event::Signal,
        };

        match event {
            Event::Gateway(Some(WsMessage::Disconnected)) | Event::Gateway(None) => {
                state.write().await.dirty = true;
                ws = None;
                // A command left running would be waiting on a socket that is
                // gone, and would hold the queue behind it. It also needs no
                // cancelling: the dropped connection fails its requests at
                // once, so it ends on its own and says why.
                queued.clear();
                let mut s = state.write().await;
                s.transcript
                    .push_notice("⚠ lost the gateway — reconnecting…");
                s.connection_lost("gateway went away");
                drop(s);
                schedule_reconnect(&mut backoff, &mut reconnect_at);
            }
            Event::Gateway(Some(message)) => {
                state.write().await.dirty = true;
                if let Some(client) = ws.as_ref() {
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
            Event::Finished(result) => {
                in_flight = None;
                if let Err(e) = result {
                    absorb_action_error(e, &state).await?;
                }
                if let (Some(next), Some(client)) = (queued.pop_front(), ws.as_ref()) {
                    in_flight = Some(spawn_action(next, &state, Arc::clone(client)));
                }
            }
            Event::Signal => {
                // Like Quit: stop reading, let the in-flight command be
                // cancelled at loop end, and the caller restores the terminal.
                state.write().await.should_quit = true;
            }
        }

        redraw(terminal, &state).await?;

        if state.read().await.should_quit {
            break;
        }
    }

    // Nothing outlives the loop: a task still waiting on a request would keep
    // the client and the state alive after the terminal has been handed back.
    if let Some(handle) = in_flight {
        handle.abort();
    }
    Ok(())
}

/// What the loop does with an action it has just read.
///
/// Pulled out of the loop because this is the whole rule, and the rule is the
/// point. Two lanes: an action that only touches local state runs *inline*,
/// even with a command in flight — the audit's acceptance for the non-blocking
/// loop is "input still editable while the gateway is slow", and a keystroke
/// that queued behind an RPC would fail it. Only actions that *start* gateway
/// work are serialized: one at a time, the rest in order, so two `/new`s
/// cannot race the session state. Quit waits for neither lane, and with no
/// gateway every action takes the offline path that existed before.
#[derive(Debug, PartialEq, Eq)]
enum Dispatch {
    /// Local only — mutate the state and keep draining.
    Edit,
    /// Nothing is in flight — run it as a task.
    Run,
    /// A command is — remember it for when that finishes.
    Queue,
    /// No gateway; only what the loop can do on its own is left.
    Offline,
    /// Set the quit flag and stop reading.
    Quit,
}

/// Whether the action belongs to the inline lane rather than a gateway command.
///
/// In composer mode these are answered from state alone. With a prompt up the
/// same keys route to the prompt handler and *can* decide over the network —
/// still worth the inline lane: a decision is the user's own next step, made
/// one keystroke at a time, and the thing that must never queue behind a
/// hung request is the typing that tells them the TUI is still alive.
fn is_local_edit(action: &TuiAction) -> bool {
    matches!(
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
    )
}

/// Decide what to do with `action`.
fn dispatch(action: &TuiAction, online: bool, busy: bool) -> Dispatch {
    if matches!(action, TuiAction::Quit) {
        Dispatch::Quit
    } else if !online {
        Dispatch::Offline
    } else if is_local_edit(action) || matches!(action, TuiAction::CopyAnswer) {
        // Copying the last answer is local work: it must never wait its turn
        // behind a hung gateway, any more than typing must.
        Dispatch::Edit
    } else if busy {
        Dispatch::Queue
    } else {
        Dispatch::Run
    }
}

/// Run one action off the loop.
///
/// The loop's own work is polling input and painting. Everything else is a
/// round-trip, and a round-trip must not hold either up: one request can take
/// the full timeout, and `/new` is three of them in a row.
fn spawn_action(
    action: TuiAction,
    state: &Arc<RwLock<AppState>>,
    client: Arc<WsClient>,
) -> JoinHandle<Result<(), TuiError>> {
    let state = Arc::clone(state);
    tokio::spawn(async move { handle_action(action, &state, &client).await })
}

/// Resolve when the in-flight command finishes — never, if there is none.
async fn settle(in_flight: &mut Option<JoinHandle<Result<(), TuiError>>>) -> Result<(), TuiError> {
    match in_flight.as_mut() {
        Some(handle) => match handle.await {
            Ok(result) => result,
            // The task panicked or was cancelled. Nothing useful to do beyond
            // saying so; the loop carries on.
            Err(e) => Err(TuiError::WebSocket(format!("a command task ended early: {e}"))),
        },
        None => std::future::pending().await,
    }
}

/// Report a failed action, or end the session if it is not survivable.
///
/// A command that fails is a line of output. Only a terminal that cannot be
/// drawn to, or an internal bug, leaves the loop with nothing to do — and it
/// is the error itself, not a flag parked in the state, that decides which.
async fn absorb_action_error(err: TuiError, state: &Arc<RwLock<AppState>>) -> Result<(), TuiError> {
    if err.is_fatal() {
        return Err(err);
    }
    state
        .write()
        .await
        .transcript
        .push_notice(format!("✘ {err}"));
    Ok(())
}

/// Await the next gateway message, or never resolve while disconnected.
async fn next_gateway(ws: &Option<Arc<WsClient>>) -> Option<WsMessage> {
    match ws.as_ref() {
        Some(client) => client.next().await,
        None => std::future::pending().await,
    }
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
        let lines = {
            let s = state.read().await;
            blocks::to_lines(&pending, &s.active_theme)
        };
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
