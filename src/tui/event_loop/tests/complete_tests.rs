//! The `@` path-listing fetch: what it asks for, and what it remembers.

use std::sync::Arc;
use std::time::Duration;

use super::support::{eventually_async, state_and_client};
use crate::tui::event_loop::complete::ensure_path_listing;
use crate::tui::state::CompletionKind;
use crate::tui::test_gateway::TestGateway;

/// Type into the buffer through the same door a keystroke uses.
async fn type_input(state: &Arc<tokio::sync::RwLock<crate::tui::state::AppState>>, text: &str) {
    let mut s = state.write().await;
    for c in text.chars() {
        s.insert_char(c);
    }
}

fn listings(gateway: &TestGateway) -> usize {
    gateway
        .requests()
        .iter()
        .filter(|r| r.method == "workspace.list")
        .count()
}

#[tokio::test]
async fn typing_an_at_token_fills_the_cache_from_the_gateway() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let client = Arc::new(client);

    type_input(&state, "@sr").await;
    ensure_path_listing(&state, &client).await;

    eventually_async(
        || {
            let state = Arc::clone(&state);
            async move {
                !state
                    .read()
                    .await
                    .path_completions
                    .entries_for("")
                    .unwrap_or(&[])
                    .is_empty()
            }
        },
        "the root listing to arrive",
    )
    .await;

    let names: Vec<String> = {
        let s = state.read().await;
        s.completions()
            .iter()
            .map(|c| c.name().to_string())
            .collect()
    };
    assert_eq!(names, vec!["src"], "the reply became a candidate, filtered by the leaf");

    // The reply also marks the state dirty, or the loop would never repaint it.
    assert!(state.read().await.dirty, "arrival asks for a repaint");
}

/// The request key is the directory, so walking through one directory's leaves
/// costs one request rather than one per keystroke.
#[tokio::test]
async fn a_directory_is_asked_for_once() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let client = Arc::new(client);

    for text in ["@", "@s", "@sr", "@src"] {
        state.write().await.clear_input();
        type_input(&state, text).await;
        ensure_path_listing(&state, &client).await;
    }
    // Let whichever request was issued come back.
    eventually_async(
        || {
            let state = Arc::clone(&state);
            async move {
                state
                    .read()
                    .await
                    .path_completions
                    .entries_for("")
                    .is_some()
            }
        },
        "the listing to arrive",
    )
    .await;

    assert_eq!(listings(&gateway), 1, "one directory, one request");
}

#[tokio::test]
async fn a_directory_the_gateway_refuses_is_remembered_as_unavailable() {
    let gateway = TestGateway::start().await;
    gateway.fail_with("workspace.list", "READ_FAILED");
    let (state, client) = state_and_client(&gateway).await;
    let client = Arc::new(client);

    type_input(&state, "@secret/").await;
    ensure_path_listing(&state, &client).await;

    eventually_async(
        || {
            let state = Arc::clone(&state);
            async move {
                state
                    .read()
                    .await
                    .path_completions
                    .is_unavailable("secret/")
            }
        },
        "the refusal to be recorded",
    )
    .await;

    let s = state.read().await;
    assert!(s.completions().is_empty(), "a refused directory offers nothing");
    assert!(s.path_completions.entries_for("secret/").is_none(), "and is not a listing");
}

/// Nothing to ask for means no request at all — the fetcher runs on every
/// keystroke, including the ones that are not completions.
#[tokio::test]
async fn ordinary_typing_asks_for_nothing() {
    let gateway = TestGateway::start().await;
    let (state, client) = state_and_client(&gateway).await;
    let client = Arc::new(client);

    for text in ["hello", "/status", "@x y"] {
        state.write().await.clear_input();
        type_input(&state, text).await;
        assert!(
            !matches!(state.read().await.completion_kind(), Some(CompletionKind::Path { .. })),
            "{text:?} is not a path token"
        );
        ensure_path_listing(&state, &client).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(listings(&gateway), 0, "no path token, no listing");
}
