//! Cross-layer isolation: per-user personality files and session ownership.
//!
//! The unit tests cover each mechanism in place (a handler, a store method, the
//! `PersonalityMemory` file layout). This file drives the two of them together
//! through the library API an embedder actually holds, from a clean temp
//! directory, so a change that re-wires one layer past the other shows up here
//! rather than in front of a user:
//!
//! 1. `PersonalityMemory` keeps `USER.md` / `MEMORY.md` per principal while the
//!    agent's own identity files stay shared.
//! 2. `SessionStore` records who owns a conversation and can list by owner.

use syscity::agent::session_store::{SessionMetadata, SessionStore};
use syscity::memory::{MemoryType, PersonalityMemory};

// ── Personality files ────────────────────────────────────────────────────────

/// Case 1 — two users write `USER.md` into the same agent's memory directory
/// and each reads back only their own.
#[tokio::test]
async fn personal_files_do_not_leak_between_users() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().to_path_buf();

    let alice = PersonalityMemory::with_dir_for_user(base.clone(), Some("alice"))
        .await
        .expect("alice memory");
    let bob = PersonalityMemory::with_dir_for_user(base.clone(), Some("bob"))
        .await
        .expect("bob memory");

    alice
        .write(MemoryType::User, "alice likes tea")
        .await
        .expect("alice writes");
    bob.write(MemoryType::User, "bob likes coffee")
        .await
        .expect("bob writes");

    assert_eq!(alice.read(MemoryType::User).await.unwrap(), "alice likes tea");
    assert_eq!(bob.read(MemoryType::User).await.unwrap(), "bob likes coffee");

    // On disk the files are genuinely separate, not one file read with a
    // different lens.
    assert!(base.join("users/alice/USER.md").exists());
    assert!(base.join("users/bob/USER.md").exists());
    assert!(
        !base.join("USER.md").exists(),
        "a partitioned write must not also land in the shared location"
    );
}

/// Case 2 — the agent's own identity files stay shared, because they describe
/// the agent, not the person talking to it.
#[tokio::test]
async fn identity_files_remain_shared_across_users() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().to_path_buf();

    let alice = PersonalityMemory::with_dir_for_user(base.clone(), Some("alice"))
        .await
        .expect("alice memory");
    let bob = PersonalityMemory::with_dir_for_user(base.clone(), Some("bob"))
        .await
        .expect("bob memory");

    alice
        .write(MemoryType::Soul, "be kind")
        .await
        .expect("alice writes soul");

    assert_eq!(
        bob.read(MemoryType::Soul).await.unwrap(),
        "be kind",
        "SOUL.md belongs to the agent, so bob must see what alice wrote"
    );
    assert!(base.join("SOUL.md").exists());
    assert!(
        !base.join("users/bob/SOUL.md").exists(),
        "an identity file must not be duplicated per user"
    );
}

/// Case 3 — a single-operator deployment (`shared` / `anonymous`) keeps the
/// flat layout, so upgrading does not strand existing memory behind a new
/// `users/` subdirectory.
#[tokio::test]
async fn shared_identities_keep_the_flat_layout() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().to_path_buf();

    for who in [None, Some("shared"), Some("anonymous")] {
        let memory = PersonalityMemory::with_dir_for_user(base.clone(), who)
            .await
            .expect("memory");
        memory
            .write(MemoryType::User, "the operator prefers terse answers")
            .await
            .expect("write");
        assert!(base.join("USER.md").exists(), "{who:?} must keep the flat layout");
        assert!(!base.join("users").exists(), "{who:?} must not create a per-user directory");
    }
}

/// Case 4 — a personal file is the user's own copy, with no silent fallback to
/// the shared one: bob must not be shown alice's `MEMORY.md`, and once bob has
/// his own the two are independent. A deployment that wants one file for
/// everybody is the unpartitioned layout, which is still what `shared` gets.
#[tokio::test]
async fn a_personal_file_does_not_fall_back_to_the_shared_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().to_path_buf();

    // Shared memory: what the deployment had before partitioning.
    let shared = PersonalityMemory::with_dir_for_user(base.clone(), None)
        .await
        .expect("shared memory");
    shared
        .write(MemoryType::Memory, "shared view of the day")
        .await
        .expect("shared write");

    // Alice has her own copy; bob does not.
    let alice = PersonalityMemory::with_dir_for_user(base.clone(), Some("alice"))
        .await
        .expect("alice memory");
    alice
        .write(MemoryType::Memory, "alice's view of the day")
        .await
        .expect("alice write");
    let bob = PersonalityMemory::with_dir_for_user(base.clone(), Some("bob"))
        .await
        .expect("bob memory");

    assert_eq!(alice.read(MemoryType::Memory).await.unwrap(), "alice's view of the day");
    assert_eq!(
        bob.read(MemoryType::Memory).await.unwrap(),
        "",
        "bob must not be handed the shared file as if it were his own"
    );

    // The unpartitioned reader still sees the shared copy, so nothing was
    // moved out from under the single-operator layout.
    assert_eq!(shared.read(MemoryType::Memory).await.unwrap(), "shared view of the day");
}

/// Case 5 — dated fragments are the exception: they merge, and a personal
/// fragment of the same name *shadows* the shared one, so a person sees one
/// entry per day rather than the same day twice with different content.
#[tokio::test]
async fn a_personal_fragment_shadows_the_shared_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().to_path_buf();

    // Shared fragments: the team's diary as it stood before partitioning.
    tokio::fs::create_dir_all(base.join("memory"))
        .await
        .expect("shared fragment dir");
    tokio::fs::write(base.join("memory/2026-01-01.md"), "shared new year")
        .await
        .expect("shared fragment");
    tokio::fs::write(base.join("memory/2026-01-02.md"), "shared second day")
        .await
        .expect("shared fragment");

    let alice = PersonalityMemory::with_dir_for_user(base.clone(), Some("alice"))
        .await
        .expect("alice memory");
    // Alice's own note for one of the days, plus one of her own.
    let alice_dir = base
        .join("users")
        .join(alice.user_scope_component().expect("component"))
        .join("memory");
    tokio::fs::create_dir_all(&alice_dir)
        .await
        .expect("alice fragment dir");
    tokio::fs::write(alice_dir.join("2026-01-01.md"), "alice's new year")
        .await
        .expect("alice fragment");
    tokio::fs::write(alice_dir.join("2026-01-03.md"), "alice's third day")
        .await
        .expect("alice fragment");

    let fragments = alice.load_memory_fragments().await;
    let names: Vec<&str> = fragments.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["2026-01-01.md", "2026-01-02.md", "2026-01-03.md"],
        "each day appears once, in date order: {names:?}"
    );
    assert_eq!(fragments[0].1, "alice's new year", "personal shadows shared");
    assert_eq!(fragments[1].1, "shared second day", "shared is still merged in");
    assert_eq!(fragments[2].1, "alice's third day", "personal-only day is added");
}

/// Case 6 — a hostile user id cannot escape the memory root. Whatever the
/// escaping scheme, the file must live under `users/`, not beside it.
#[tokio::test]
async fn a_path_shaped_user_id_cannot_escape_the_memory_root() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().to_path_buf();
    let root = dir.path().canonicalize().expect("canonical root");

    for who in ["../../etc", "..", "/etc/passwd", "a/../../b"] {
        let memory = PersonalityMemory::with_dir_for_user(base.clone(), Some(who))
            .await
            .expect("memory");
        memory
            .write(MemoryType::User, "contained")
            .await
            .expect("write");

        // The escaped component is what the instance resolved to; the file it
        // wrote must sit beneath it, and the whole of it beneath the root.
        let component = memory
            .user_scope_component()
            .expect("a user-scoped instance has a component");
        let personal = base.join("users").join(component);
        let canonical = personal.canonicalize().unwrap_or_else(|_| personal.clone());
        assert!(
            canonical.starts_with(&root),
            "user id {who:?} escaped the memory root: {canonical:?}"
        );
        assert!(
            personal.join("USER.md").exists(),
            "user id {who:?} wrote outside of its own directory"
        );
    }
}

// ── Session ownership ────────────────────────────────────────────────────────

async fn store() -> SessionStore {
    SessionStore::new(":memory:")
        .await
        .expect("in-memory session store")
}

fn meta(session: &str, owner: Option<&str>) -> SessionMetadata {
    SessionMetadata::new(session, "", "ws", owner.unwrap_or("")).with_user_id(owner)
}

/// Case 7 — a store file can be listed by owner, and a different owner's
/// listing does not contain the first owner's sessions.
#[tokio::test]
async fn sessions_are_listable_by_owner() {
    let store = store().await;
    for (session, owner) in [
        ("alice-1", Some("alice")),
        ("alice-2", Some("alice")),
        ("bob-1", Some("bob")),
        ("anon-1", None),
    ] {
        store
            .save_session(session, &meta(session, owner), "{}")
            .await
            .expect("save");
    }

    let alice: Vec<String> = store
        .find_sessions_for_user(None, None, None, false, Some("alice"))
        .await
        .expect("list alice")
        .into_iter()
        .map(|m| m.session_id)
        .collect();
    assert_eq!(alice.len(), 2, "alice's sessions: {alice:?}");
    assert!(alice.iter().all(|id| id.starts_with("alice-")));

    // An unfiltered listing still sees everything — the filter narrows, it
    // does not change what the deployment recorded.
    let all = store
        .find_sessions(None, None, None, false)
        .await
        .expect("list all");
    assert_eq!(all.len(), 4);
}

/// Case 8 — a background save with no user must not erase a recorded owner.
/// The session was created by a person; a later agent-side write does not
/// change who it belongs to.
#[tokio::test]
async fn a_userless_save_keeps_the_recorded_owner() {
    let store = store().await;
    store
        .save_session("s1", &meta("s1", Some("alice")), "{}")
        .await
        .expect("first save");

    // A later save built without an owner, the way a background task would.
    let mut later = SessionMetadata::new("s1", "", "ws", "system");
    later.name = Some("renamed".to_string());
    store
        .save_session("s1", &later, "{}")
        .await
        .expect("second save");

    let loaded = store.load_session("s1").await.unwrap().unwrap();
    assert_eq!(
        loaded.metadata.user_id.as_deref(),
        Some("alice"),
        "the recorded owner must survive a user-less save"
    );
    assert_eq!(loaded.metadata.name.as_deref(), Some("renamed"), "the rest of the save applied");
}

/// Case 9 — the shared identities never become owners, even when a caller
/// passes one explicitly. They mean "this deployment has no per-user
/// identity", so recording one would claim a person who does not exist.
#[tokio::test]
async fn shared_identities_are_never_recorded_as_owners() {
    for id in ["shared", "anonymous", "ws-user", "  "] {
        let meta = SessionMetadata::new("s", "", "ws", "x").with_user_id(Some(id));
        assert_eq!(meta.user_id, None, "{id:?} must not be an owner");
    }
}
