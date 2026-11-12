//! Regression tests for message persistence across *every* turn path.
//!
//! The double-write bug (`remember_message` + `store_message` on the same
//! store) and the replayed-user-message bug were both fixed in
//! `persist_user_message` / `persist_assistant_message`, with unit tests in
//! `src/agent/engine/tests.rs` covering the normal path. The cache-hit path is
//! a second, separate copy of that persistence code — and it had its own
//! variant of the defect: it called `store_message` directly, so a cached turn
//! was never indexed for session search and never invalidated the context
//! cache.
//!
//! These tests live in a separate target on purpose. A unit test in `src/`
//! only proves the branch you remembered to write a test against; this one
//! drives the public `Agent::process_message` twice through the same
//! conversation and asserts on the store, so any future path that forgets to
//! persist (or persists twice) fails here without anyone editing this file.

// The workspace denies `unwrap_used`/`expect_used`; in a test the panic *is*
// the assertion, and the repo's precedent for test targets (`tests/tui_pty.rs`)
// is a file-level allow rather than unwrapping every fixture by hand.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use async_trait::async_trait;
use syscity::agent::Agent;
use syscity::agent::AgentConfig;
use syscity::channels::IncomingMessage;
use syscity::memory::{ChatHistoryStore, DatabaseStore, MemoryManager, MemoryManagerConfig};
use syscity::providers::{
    CompletionRequest, CompletionResponse, CompletionStream, Message, Provider, Usage,
};
use syscity::tools::ToolRegistry;

mod common;

const CONV: &str = "conv-persistence";
const USER: &str = "u1";

/// A provider that always answers the same way, so the second identical
/// message can be served from the response cache.
struct FixedProvider;

#[async_trait]
impl Provider for FixedProvider {
    fn name(&self) -> &str {
        "fixed-provider"
    }
    fn default_model(&self) -> &str {
        "fixed-model"
    }
    fn supports_tools(&self) -> bool {
        false
    }
    fn max_context(&self) -> usize {
        128_000
    }

    async fn complete(&self, _request: CompletionRequest) -> syscity::Result<CompletionResponse> {
        Ok(CompletionResponse {
            message: Message::assistant("the answer"),
            model: self.default_model().to_string(),
            usage: Some(Usage::default()),
            finish_reason: Some("stop".to_string()),
        })
    }

    async fn stream(&self, _request: CompletionRequest) -> syscity::Result<CompletionStream> {
        unimplemented!("non-streaming test provider")
    }

    async fn health_check(&self) -> syscity::Result<bool> {
        Ok(true)
    }

    async fn set_credential(
        &self,
        _credential: syscity::model_router::Credential,
    ) -> syscity::Result<()> {
        Ok(())
    }
}

async fn build_agent(store: Arc<DatabaseStore>) -> Agent {
    // Integration tests link the library without `cfg(test)`, so no root is
    // installed for them — the agent's path resolution would panic by design.
    common::install_test_root();
    Agent::new(AgentConfig::default(), Arc::new(FixedProvider), Arc::new(ToolRegistry::new()))
        .with_memory_manager(Arc::new(MemoryManager::new(
            store.clone(),
            store.clone(),
            MemoryManagerConfig::default(),
        )))
}

async fn rows(store: &DatabaseStore) -> Vec<String> {
    store
        .get_conversation_history(CONV, 50)
        .await
        .expect("read history")
        .into_iter()
        .map(|m| format!("{}:{}", m.role, m.content))
        .collect()
}

/// One message in, one row out — on the first turn and on every turn after.
#[tokio::test]
async fn a_turn_adds_exactly_one_user_and_one_assistant_row() {
    let store = Arc::new(DatabaseStore::new_in_memory().await.unwrap());
    let agent = build_agent(store.clone()).await;

    agent
        .process_message(IncomingMessage::new(USER, CONV, "hello"))
        .await
        .unwrap();
    assert_eq!(
        rows(&store).await,
        vec!["user:hello".to_string(), "assistant:the answer".to_string()],
        "one turn must write exactly one user row and one assistant row"
    );

    agent
        .process_message(IncomingMessage::new(USER, CONV, "again"))
        .await
        .unwrap();
    assert_eq!(
        rows(&store).await,
        vec![
            "user:hello".to_string(),
            "assistant:the answer".to_string(),
            "user:again".to_string(),
            "assistant:the answer".to_string(),
        ],
        "the second turn must append, not rewrite or duplicate"
    );
}

/// A message served from the response cache is still a turn and must be
/// persisted through the same path — the cache-hit branch used to call
/// `store_message` directly, which skipped session-search indexing and the
/// context-cache invalidation.
#[tokio::test]
async fn a_cached_turn_is_persisted_like_any_other() {
    let store = Arc::new(DatabaseStore::new_in_memory().await.unwrap());
    let agent = build_agent(store.clone()).await;

    // Long enough to pass the cache's "too short to be cacheable" guard, and
    // phrased as a factual question so the classifier says CACHE.
    let question = "Please explain how a binary search tree works in general terms.";

    agent
        .process_message(IncomingMessage::new(USER, CONV, question))
        .await
        .unwrap();
    let first = rows(&store).await;

    agent
        .process_message(IncomingMessage::new(USER, CONV, question))
        .await
        .unwrap();
    let second = rows(&store).await;

    // Whatever the cache decided, the invariant must hold: the second turn
    // adds exactly one user row and one assistant row.
    assert_eq!(
        second.len(),
        first.len() + 2,
        "a repeated message must add its own two rows, got {second:?}"
    );
    assert_eq!(
        second
            .iter()
            .filter(|r| **r == format!("user:{question}"))
            .count(),
        2,
        "the repeated question must appear once per turn, got {second:?}"
    );
}
