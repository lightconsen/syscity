//! Reconnecting: backoff, the socket coming back, and what was missed.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use crate::tui::app::Endpoint;
use crate::tui::gateway_calls::{self as gw, HistoryMessage};
use crate::tui::retry::Backoff;
use crate::tui::state::{AppState, ConnectionState, Interruption};
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ui::blocks;
use crate::tui::ws_client::WsClient;

/// Schedule the next reconnect attempt.
pub(super) fn schedule_reconnect(backoff: &mut Backoff, reconnect_at: &mut Option<Instant>) {
    let delay = backoff.next_delay();
    *reconnect_at = Some(Instant::now() + delay);
}

/// Attempt to reconnect, reporting the outcome into the transcript.
pub(super) async fn try_reconnect(
    state: &Arc<RwLock<AppState>>,
    ws: &mut Option<Arc<WsClient>>,
    endpoint: &Endpoint,
    backoff: &mut Backoff,
    reconnect_at: &mut Option<Instant>,
) {
    *reconnect_at = None;
    let attempt = backoff.attempt() + 1;
    match WsClient::connect(&endpoint.url, &endpoint.auth, &["chat", "read", "write"]).await {
        Ok((client, hello)) => {
            *ws = Some(Arc::new(client));
            backoff.reset();
            let mut s = state.write().await;
            s.connection = ConnectionState::Connected {
                features: hello.features,
                scopes_granted: hello.scopes_granted,
                server_version: hello.server.version,
            };
            s.dirty = true;
            s.transcript.push_notice("── reconnected ──");
            let session = s.current_session.clone();
            let interrupted = s.interrupted.take();
            drop(s);
            let Some(id) = session else {
                return;
            };
            let Some(client) = ws.as_ref() else {
                return;
            };
            if let Err(e) = gw::sessions_subscribe(client, &id).await {
                state
                    .write()
                    .await
                    .transcript
                    .push_notice(format!("⚠ reconnected but not subscribed to {id}: {e}"));
                return;
            }
            reconcile_after_reconnect(&id, interrupted, state, client).await;
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

/// How many messages back a reconnect looks for what it missed.
const RECONCILE_TAIL: usize = 20;

/// Print what the gateway produced while the connection was down.
///
/// The transcript is the record, so nothing already printed is printed again:
/// only messages the gateway wrote *after* the socket went away, which by
/// definition the TUI never received. That is what turns "the run was
/// interrupted" — true when we last looked — into what actually became of it.
///
/// The comparison is our clock against the gateway's `created_at`. Those agree
/// when the gateway is local, which is the ordinary case, and the reconnect
/// backoff is half a second and up — far more than the skew between two
/// machines kept in sync. A skewed clock can only make the window a little
/// wide or narrow; it cannot make the TUI reprint what it already showed,
/// because nothing it showed has a timestamp after the disconnect.
async fn reconcile_after_reconnect(
    session: &str,
    interrupted: Option<Interruption>,
    state: &Arc<RwLock<AppState>>,
    client: &WsClient,
) {
    let Some(interrupted) = interrupted else {
        return;
    };
    let messages = match gw::chat_history(client, session, RECONCILE_TAIL, None).await {
        Ok((messages, _)) => messages,
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("⚠ could not check what arrived while offline: {e}"));
            return;
        }
    };
    let missed: Vec<HistoryMessage> = messages
        .into_iter()
        .filter(|m| m.timestamp_ms.is_some_and(|ts| ts > interrupted.since_ms))
        .collect();

    let mut lines = Vec::new();
    if missed.is_empty() {
        if interrupted.run_in_flight {
            lines.push(TranscriptLine::new(
                LineKind::Notice,
                "── the run did not finish while offline — nothing arrived ──",
            ));
        }
    } else {
        lines.push(blocks::rule(&format!("while offline: {} message(s)", missed.len())));
        lines.extend(blocks::history_lines(&missed));
    }
    if !lines.is_empty() {
        state.write().await.transcript.push(lines);
    }
}
