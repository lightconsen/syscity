//! Reconnecting: what was missed, and how the backoff grows.

use std::sync::Arc;
use std::time::Instant;

use crate::tui::event_loop::reconnect::{schedule_reconnect, try_reconnect};
use crate::tui::retry::Backoff;
use crate::tui::state::Interruption;
use crate::tui::test_gateway::TestGateway;

use super::support::*;

/// A reconnect shows what arrived while the connection was down.
///
/// The notice used to say only "output produced while offline was not
/// received" and leave it there — so a turn that finished while the socket
/// was gone was indistinguishable from one that died. The client now asks
/// the gateway what it missed: messages written after the disconnect,
/// which by definition it never saw, so nothing already in the transcript
/// is printed twice.
#[tokio::test]
async fn a_reconnect_shows_what_arrived_while_offline() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let mut ws = Some(Arc::new(client));
    let disconnected_at = 1_757_000_000_000_i64;
    {
        let mut s = state.write().await;
        s.current_session = Some("s1".to_string());
        s.interrupted = Some(Interruption {
            since_ms: disconnected_at,
            run_in_flight: true,
        });
    }
    gateway.with_history(vec![
        // Written before the socket went away: already in the transcript.
        serde_json::json!({
            "id": "msg_old",
            "role": "user",
            "content": "already seen",
            "timestamp": disconnected_at - 1_000,
        }),
        // Written after: this is what the TUI never received.
        serde_json::json!({
            "id": "msg_new",
            "role": "assistant",
            "content": "the answer you missed",
            "timestamp": disconnected_at + 1_000,
        }),
    ]);

    let mut backoff = Backoff::new();
    let mut reconnect_at = None;
    try_reconnect(&state, &mut ws, &test_endpoint(gateway.port), &mut backoff, &mut reconnect_at)
        .await;

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(lines.iter().any(|l| l.contains("the answer you missed")), "got {lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("already seen")),
        "and nothing it already had: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("while offline")), "framed as such: {lines:?}");
    assert!(
        state.read().await.interrupted.is_none(),
        "the window is spent once it has been answered"
    );
}

/// A reconnect that finds nothing says that too — a run was in flight, and
/// "nothing arrived" is the answer to "what happened to it".
#[tokio::test]
async fn a_reconnect_that_finds_nothing_closes_the_question() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let mut ws = Some(Arc::new(client));
    {
        let mut s = state.write().await;
        s.current_session = Some("s1".to_string());
        s.interrupted = Some(Interruption {
            since_ms: 1_757_000_000_000,
            run_in_flight: true,
        });
    }

    let mut backoff = Backoff::new();
    let mut reconnect_at = None;
    try_reconnect(&state, &mut ws, &test_endpoint(gateway.port), &mut backoff, &mut reconnect_at)
        .await;

    let lines: Vec<String> = state
        .write()
        .await
        .transcript
        .take_flushable()
        .into_iter()
        .map(|l| l.text)
        .collect();
    assert!(lines.iter().any(|l| l.contains("did not finish")), "got {lines:?}");
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
