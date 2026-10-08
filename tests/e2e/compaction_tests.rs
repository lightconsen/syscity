//! E2E: durable context compaction (#10).
//!
//! Exercises the overflow-retry path through a real gateway:
//!
//! 1. **Gateway A** accumulates a conversation, then a message whose stream
//!    call is injected with a context-length error triggers
//!    `compact_context_forced` → the agent summarizes the middle and persists a
//!    `conversation_compactions` row anchored on the tail boundary, then
//!    retries the request successfully.
//! 2. **Gateway B** is started on the *same* sqlite file with the same mock
//!    (shared history). A follow-up chat on the same session rehydrates from
//!    the durable record as `[summary + tail]` instead of replaying full
//!    history.
//!
//! Requires `sqlx` (already a dependency) to inspect the sqlite table directly.

use super::*;
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use syscity::error::SyscityError;

/// Context-length error a real provider would surface when the request
/// exceeds the model's actual window.
fn context_length_error() -> SyscityError {
    SyscityError::ExternalService {
        source: "Test provider: this model's maximum context length is 2048 tokens".into(),
        cause: None,
    }
}

/// Mock provider for the compaction flow.
///
/// - Normal chats get a **unique** assistant reply (`assistant-reply-N`); the
///   uniqueness guarantees the persisted boundary anchor resolves to a single
///   `chat_messages` row via `MAX(rowid)`.
/// - The first request whose last user message is `TRIGGER_COMPACTION` fails
///   with a context-length error (once) — that is what trips the overflow
///   retry. Subsequent requests succeed.
/// - A request that already carries a `compaction_summary` (i.e. the retried
///   request, or a rehydrated context) is answered with `rehydrated-reply` so
///   the test can distinguish it in history.
fn compaction_mock_provider() -> MockProvider {
    let reply_counter = Arc::new(AtomicUsize::new(0));
    let fired = Arc::new(AtomicBool::new(false));

    let cb_counter = Arc::clone(&reply_counter);
    let cb_fired = Arc::clone(&fired);
    MockProvider::new()
        .with_callback(move |messages| {
            // Cache-check prompt.
            if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
                return ProviderMessage::assistant("NOCACHE");
            }
            // Compaction already applied / rehydrated.
            if messages
                .iter()
                .any(|m| m.name.as_deref() == Some("compaction_summary"))
            {
                return ProviderMessage::assistant("rehydrated-reply");
            }
            let n = cb_counter.fetch_add(1, Ordering::Relaxed);
            ProviderMessage::assistant(format!("assistant-reply-{}", n))
        })
        .with_error_callback(move |messages| {
            // `Context::to_messages` appends a labeled `state_snapshot` user
            // message at the request tail; skip it so the trigger keys on the
            // human's actual turn (matching production semantics).
            let last_user = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User && m.name.as_deref() != Some("state_snapshot"));
            if !cb_fired.load(Ordering::Relaxed)
                && last_user
                    .map(|m| m.content.contains("TRIGGER_COMPACTION"))
                    .unwrap_or(false)
            {
                cb_fired.store(true, Ordering::Relaxed);
                return Some(context_length_error());
            }
            None
        })
}

/// Start a gateway on a *specific* sqlite file, returning the handle so the
/// test can `stop()` it and reuse the file from a second gateway.
async fn start_gateway_with_mock_on_db(
    port: u16,
    mock: MockProvider,
    db_path: &Path,
) -> Arc<Gateway> {
    let mut config = test_config(port, false);
    // Point at the caller's db file (test_config's default per-port file is
    // removed by it; we deliberately do NOT remove ours here so gateway B can
    // reuse it).
    config.storage.database_url = Some(format!("sqlite:{}", db_path.display()));
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = Arc::new(new_test_gateway(config).await);
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock, "mock-model").await;

    let g = Arc::clone(&gateway);
    tokio::spawn(async move {
        let _ = g.start().await;
    });
    wait_for_gateway_listener(port).await;
    gateway
}

/// Poll until the gateway's WS listener accepts connections.
async fn wait_for_gateway_listener(port: u16) {
    let url = format!("ws://127.0.0.1:{}/ws", port);
    let deadline = tokio::time::Instant::now() + GATEWAY_START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(_)) = timeout(Duration::from_secs(5), connect_async(&url)).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    dump_captured_logs();
    panic!("gateway on port {port} did not become ready in time");
}

/// Send a chat message and wait for its `chat.final` event.
async fn send_chat_and_wait_final(client: &mut FrontendSimulator, sid: &str, msg: &str) {
    client.send_chat(sid, msg).await;
    let final_evt = client.wait_for_event("chat.final", 30).await;
    assert!(final_evt.is_some(), "chat.final not received for: {msg}");
}

#[tokio::test]
#[serial]
async fn test_compaction_overflow_retry_persists_and_rehydrates() {
    let db_path = std::env::temp_dir().join("syscity_e2e_compaction.db");
    let _ = std::fs::remove_file(&db_path);

    let mock = compaction_mock_provider();

    // ── Gateway A: accumulate context, then trip the overflow retry ────────
    let port_a = free_port();
    let gateway_a = start_gateway_with_mock_on_db(port_a, mock.clone(), &db_path).await;
    let mut client = FrontendSimulator::connect(port_a).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;

    for i in 0..6 {
        send_chat_and_wait_final(&mut client, &sid, &format!("message {i}")).await;
    }

    // The triggering message fails once with ContextLength at stream setup,
    // which must compact (persisting a boundary) and retry successfully.
    send_chat_and_wait_final(&mut client, &sid, "TRIGGER_COMPACTION").await;

    // 1) A retried request carrying the compaction summary must exist.
    let history = mock.history();
    let retried = history.iter().rev().find(|r| {
        r.messages
            .iter()
            .any(|m| m.name.as_deref() == Some("compaction_summary"))
    });
    assert!(
        retried.is_some(),
        "no retried request contained a compaction_summary message (history={} requests)",
        history.len()
    );
    let retried = retried.expect("checked above");
    assert!(
        retried
            .messages
            .iter()
            .any(|m| m.content.contains("TRIGGER_COMPACTION")),
        "retried request lost the triggering user message"
    );

    // 2) A durable record must exist in sqlite with a non-empty summary.
    let pool = SqlitePool::connect(&format!("sqlite:{}", db_path.display()))
        .await
        .expect("open test db");
    let (boundary_role, boundary_content, summary): (String, String, String) = sqlx::query_as(
        "SELECT boundary_role, boundary_content, summary FROM conversation_compactions \
         WHERE conversation_id = ?",
    )
    .bind(&sid)
    .fetch_one(&pool)
    .await
    .expect("conversation_compactions row missing after compaction");
    assert!(!summary.is_empty(), "persisted summary must not be empty");
    assert_eq!(boundary_role, "assistant", "boundary anchor role mismatch");
    assert!(
        boundary_content.starts_with("assistant-reply-"),
        "boundary anchor content unexpected: {boundary_content}"
    );
    let tail: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM chat_messages WHERE conversation_id = ? ORDER BY rowid DESC LIMIT 3",
    )
    .bind(&sid)
    .fetch_all(&pool)
    .await
    .expect("read tail");
    // Sanity: the persisted tail ends at the triggering message.
    assert!(
        tail.iter().any(|(_, c)| c.contains("TRIGGER_COMPACTION")),
        "persisted history should still contain the triggering message"
    );

    // ── Gateway B: rehydrate the same conversation from the record ─────────
    gateway_a.stop().await.expect("gateway A stop");
    let port_b = free_port();
    let gateway_b = start_gateway_with_mock_on_db(port_b, mock.clone(), &db_path).await;
    let mut client_b = FrontendSimulator::connect(port_b).await;
    // No create_session: reuse gateway A's session id so the agent rebuilds
    // THAT conversation's context.
    send_chat_and_wait_final(&mut client_b, &sid, "follow up").await;

    let history_b = mock.history();
    let rehydrated = history_b
        .iter()
        .rev()
        .find(|r| r.messages.iter().any(|m| m.content.contains("follow up")));
    assert!(
        rehydrated.is_some(),
        "gateway B request not found in mock history ({} requests)",
        history_b.len()
    );
    let rehydrated = rehydrated.expect("checked above");
    let msgs = &rehydrated.messages;

    // 3) The rehydrated request starts with the durable summary, then the tail.
    assert!(
        msgs.iter()
            .any(|m| m.name.as_deref() == Some("compaction_summary")),
        "gateway B request did not rehydrate the compaction summary"
    );
    let summary_pos = msgs
        .iter()
        .position(|m| m.name.as_deref() == Some("compaction_summary"))
        .expect("checked above");
    let user_contents: Vec<&str> = msgs
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        user_contents.iter().any(|c| c.contains("follow up")),
        "follow-up user message missing from rehydrated request"
    );
    assert!(
        !user_contents.iter().any(|c| c.contains("message 0")),
        "pre-boundary history was replayed instead of [summary + tail]"
    );
    assert!(summary_pos < msgs.len() - 1, "summary must be followed by the tail");
    // The persisted tail ends at TRIGGER_COMPACTION, so it should appear in
    // the rehydrated request too.
    assert!(
        user_contents
            .iter()
            .any(|c| c.contains("TRIGGER_COMPACTION")),
        "rehydrated tail should include the boundary-era messages"
    );

    gateway_b.stop().await.expect("gateway B stop");
    let _ = std::fs::remove_file(&db_path);
}
