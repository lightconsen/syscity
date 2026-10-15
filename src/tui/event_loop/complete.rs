//! Fetching the directory listings `@` completion reads.
//!
//! The composer's completion path is synchronous and offline-safe, and it stays
//! that way: Tab reads whatever is cached and nothing else. This module is the
//! other half — it notices that the cursor is in a path token naming a
//! directory nobody has listed yet, and asks the gateway for it in the
//! background.
//!
//! The request key is the *directory*, not the keystroke. Typing `@src/ma`
//! reuses the listing fetched when the `/` was typed, so walking a tree costs
//! one round trip per directory actually descended into — the same laziness the
//! WebUI's file tree has.
//!
//! Two failure modes are deliberately different. A gateway *refusal* (it
//! answered, and the answer was no) is cached as unavailable, so typing does
//! not re-ask on every keystroke. A transport failure is not cached at all: the
//! connection is gone, and after a reconnect the next keystroke should try
//! again.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::tui::gateway_calls as gw;
use crate::tui::state::{AppState, CompletionKind};
use crate::tui::ws_client::WsClient;

/// Ask for the listing the cursor's `@` token needs, if it is missing.
///
/// Returns immediately: the request is spawned, and its reply marks the state
/// dirty so the next repaint shows the candidates. Nothing here is awaited by
/// the loop, which is what keeps Tab and typing off the network.
pub(super) async fn ensure_path_listing(state: &Arc<RwLock<AppState>>, client: &Arc<WsClient>) {
    let (dir, agent_id, generation) = {
        let mut s = state.write().await;
        let Some(CompletionKind::Path { dir, .. }) = s.completion_kind() else {
            return;
        };
        // `None` means it is already cached, or already on its way.
        let Some(generation) = s.path_completions.begin_fetch(&dir) else {
            return;
        };
        (dir, s.current_agent.clone(), generation)
    };

    let state = Arc::clone(state);
    let client = Arc::clone(client);
    tokio::spawn(async move {
        // Detached rather than held in the loop's command lane: its whole
        // purpose is to not occupy the lane that serialises commands, and it
        // holds no terminal handle, so a shutdown cannot leave the screen half
        // restored.
        match gw::workspace_list(&client, agent_id.as_deref(), &dir).await {
            Ok(entries) => {
                let mut s = state.write().await;
                s.path_completions
                    .finish_fetch(&dir, generation, Some(entries));
                s.dirty = true;
            }
            Err(e) if is_gateway_refusal(&e) => {
                let mut s = state.write().await;
                s.path_completions.finish_fetch(&dir, generation, None);
                s.dirty = true;
            }
            Err(e) => {
                // The connection went away mid-request. Leave the directory
                // uncached so a reconnect and another keystroke retry it.
                tracing::debug!(dir, error = %e, "workspace listing failed");
                let mut s = state.write().await;
                s.path_completions.abandon_fetch(&dir);
            }
        }
    });
}

/// Did the gateway answer, or did the connection fail?
///
/// The difference decides whether the directory is remembered as unavailable:
/// a refusal is a fact about the workspace, a lost socket is not.
fn is_gateway_refusal(error: &crate::tui::error::TuiError) -> bool {
    matches!(error, crate::tui::error::TuiError::Gateway { .. })
}
