//! Memory-manager observe/retrieve path.
//!
//! `tests/integrations/memory_tests.rs` exercises the memory *tool* against a
//! store; `src/memory/manager/` — `observe_retrieve.rs` (570 lines),
//! `manager_effectiveness.rs`, `manager_compaction.rs`, `session_context.rs` —
//! had no tests of its own. The manager is what the agent actually uses to
//! remember and recall, so a break in observe (silently storing nothing) or
//! retrieve (returning nothing) was invisible to CI.
//!
//! Everything here runs against an in-memory SQLite store with **no**
//! embedder: without a pipeline the manager stores the memory with no
//! embedding and retrieval falls back to a SQL `content LIKE '%query%'`
//! match, so queries below are contiguous substrings of the stored content.

use super::*;

use syscity::memory::{DatabaseStore, MemoryManager, MemoryManagerConfig};

async fn manager() -> MemoryManager {
    let store = Arc::new(
        DatabaseStore::new_in_memory()
            .await
            .expect("in-memory store"),
    );
    MemoryManager::new(store.clone(), store, MemoryManagerConfig::default())
}

/// Case 1 — observe then retrieve round-trips a memory for its user.
#[tokio::test]
async fn observe_then_retrieve_returns_the_memory() {
    let mm = manager().await;

    let id = mm
        .observe("user-1", "Syscity routes completions to a registered provider", "fact", 0.9)
        .await
        .expect("observe must store the memory");

    let hits = mm
        .retrieve("user-1", None, "registered provider", Some(5), None)
        .await
        .expect("retrieve");
    assert_eq!(hits.len(), 1, "the stored memory must be recalled: {hits:?}");
    assert_eq!(hits[0].id.to_string(), id.to_string());

    // Memories are per-user: another user's query must not see it.
    let other = mm
        .retrieve("user-2", None, "registered provider", Some(5), None)
        .await
        .expect("retrieve");
    assert!(other.is_empty(), "a memory must not leak across users, got {other:?}");
}

/// Case 2 — forgetting removes the memory from later recalls.
#[tokio::test]
async fn forget_removes_the_memory_from_recall() {
    let mm = manager().await;

    let id = mm
        .observe("user-1", "the build is green on main", "fact", 0.5)
        .await
        .expect("observe");

    assert!(mm.forget(&id).await.expect("forget"), "forget reports the removal");
    let hits = mm
        .retrieve("user-1", None, "build is green", Some(5), None)
        .await
        .expect("retrieve");
    assert!(hits.is_empty(), "a forgotten memory must not be recalled: {hits:?}");
}

/// Case 3 — the session context splits into two halves.
///
/// Episodic turns come back in `messages`. The *injectable* text
/// (`format_for_injection`) carries only semantic memories — and retrieval is
/// **conversation-scoped** (`MemoryQuery::for_conversation` becomes
/// `AND conversation_id = ?`), while `observe` stores a memory without
/// binding it to any conversation. The consequence, pinned here: a memory
/// observed with `observe` is recallable by `retrieve` with no conversation
/// filter, but never surfaces through `session_context` for a specific
/// conversation. The manager's own observe → session_context loop does not
/// close.
#[tokio::test]
async fn session_context_is_conversation_scoped_while_observe_is_not() {
    let mm = manager().await;

    mm.remember_message("user-1", "conv-1", "user", "what is the deploy status?")
        .await
        .expect("remember user turn");
    mm.remember_message("user-1", "conv-1", "assistant", "the deploy is green")
        .await
        .expect("remember assistant turn");
    mm.observe("user-1", "the deploy pipeline requires a green build", "fact", 0.9)
        .await
        .expect("observe");

    // Episodic half: both turns are retrieved for the conversation.
    let ctx = mm
        .session_context("user-1", "conv-1", None::<String>, None)
        .await
        .expect("session context");
    assert_eq!(ctx.messages.len(), 2, "both turns: {:?}", ctx.messages);
    assert!(
        ctx.messages
            .iter()
            .any(|m| m.content.contains("deploy is green")),
        "the assistant turn must be present: {:?}",
        ctx.messages
    );

    // Injectable half: no conversation-bound memory exists, so nothing is
    // injected — turns alone are episodic, not injected.
    let injected = ctx.format_for_injection();
    assert!(
        injected.is_empty(),
        "an observed (unbound) memory must not surface for a specific conversation; \
         got {injected:?}"
    );

    // The same memory *is* recallable without a conversation filter — the
    // contrast that isolates the scoping rule.
    let hits = mm
        .retrieve("user-1", None, "green build", Some(5), None)
        .await
        .expect("retrieve");
    assert!(
        hits.iter().any(|m| m.content.contains("green build")),
        "the observed memory must be recallable without a conversation filter: {hits:?}"
    );
}

/// Case 4 — stats reflect what was observed, so an operator (or the SPA's
/// memory panel) sees the store it actually has.
#[tokio::test]
async fn stats_count_the_observed_memories() {
    let mm = manager().await;

    for (i, content) in ["alpha fact one", "beta fact two", "gamma fact three"]
        .iter()
        .enumerate()
    {
        mm.observe("user-1", *content, "fact", 0.5 + i as f32 * 0.1)
            .await
            .expect("observe");
    }

    let stats = mm.stats().await.expect("stats");
    assert!(stats.total_count >= 3, "every observed memory must be counted, got {stats:?}");
    assert_eq!(
        stats
            .count_by_type
            .get(&syscity::memory::MemoryEntryType::Fact)
            .copied()
            .unwrap_or(0),
        3,
        "the observations are typed as facts: {stats:?}"
    );
}
