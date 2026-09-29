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

/// Case 3 — the observe → session_context loop closes, and conversation
/// scoping still holds for memories that *are* bound.
///
/// `observe` stores a user-level fact with no conversation binding. Recall is
/// conversation-scoped (`MemoryQuery::for_conversation`), and the strict
/// `conversation_id = ?` filter used to exclude every unbound memory (NULL
/// equals nothing), so a fact the manager had observed could never reach a
/// session's context. Retrieval now treats an unbound memory as belonging to
/// its user — recallable from any conversation — while a memory bound to
/// another conversation stays scoped to that one.
#[tokio::test]
async fn session_context_injects_unbound_memories_but_respects_bound_ones() {
    use syscity::memory::{Memory, MemoryStore};

    let store = Arc::new(
        DatabaseStore::new_in_memory()
            .await
            .expect("in-memory store"),
    );
    let mm = MemoryManager::new(store.clone(), store.clone(), MemoryManagerConfig::default());

    // A user-level fact, observed without a conversation.
    mm.observe("user-1", "the deploy pipeline requires a green build", "fact", 0.9)
        .await
        .expect("observe");

    // A memory bound to a *different* conversation (the shape compaction
    // produces) must not leak into this one.
    store
        .store(
            Memory::new("user-1", "other conversation marker zzz-unique", "compaction")
                .with_conversation("conv-other"),
        )
        .await
        .expect("store bound memory");

    let ctx = mm
        .session_context("user-1", "conv-1", Some("deploy pipeline"), None)
        .await
        .expect("session context");
    let injected = ctx.format_for_injection();

    assert!(
        injected.contains("green build"),
        "an observed user-level fact must reach the session context: {injected:?}"
    );
    assert!(
        !injected.contains("zzz-unique"),
        "a memory bound to another conversation must stay out of this one: {injected:?}"
    );

    // Episodic half still works alongside it.
    mm.remember_message("user-1", "conv-1", "user", "what is the deploy status?")
        .await
        .expect("remember turn");
    let ctx = mm
        .session_context("user-1", "conv-1", None::<String>, None)
        .await
        .expect("session context");
    assert_eq!(ctx.messages.len(), 1, "the remembered turn: {:?}", ctx.messages);
    assert!(
        ctx.format_for_injection().contains("green build"),
        "the query-less path injects user-level memories too: {:?}",
        ctx.format_for_injection()
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
