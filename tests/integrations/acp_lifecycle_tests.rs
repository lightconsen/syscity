//! ACP sub-agent lifecycle — the happy path.
//!
//! `tests/integrations/acp_tests.rs` covers the tool surface and a lot of
//! error paths, and `src/acp/control_plane/{spawn,session,subagent_ops}.rs`
//! (~1600 lines) have no tests of their own. Nothing proved that a spawned
//! sub-agent actually *runs*: with no agent builder configured, every spawn in
//! the existing tests fails by design, so a control plane that never executed
//! a turn would look fully tested.
//!
//! These tests install a builder backed by the mock provider and drive real
//! turns through a sub-agent's session.

use super::*;

use syscity::acp::{AcpControlPlane, SpawnMode, SubagentConfig, ThreadBinding};
use syscity::agent::{Agent, AgentConfig};
use syscity::channels::IncomingMessage;
use syscity::providers::{mock::MockProvider, Message};

/// A builder that gives every sub-agent a scripted provider.
fn scripted_builder(
    replies: Vec<&'static str>,
) -> impl Fn(&str) -> syscity::Result<Agent> + Send + Sync + 'static {
    move |_subagent_id: &str| {
        let provider = Arc::new(
            MockProvider::new()
                .with_responses(replies.iter().map(|r| Message::assistant(*r)).collect()),
        );
        Ok(Agent::new(
            AgentConfig::default(),
            provider,
            Arc::new(syscity::tools::ToolRegistry::new()),
        ))
    }
}

/// Case 1 — a spawned sub-agent runs turns in its session and answers with
/// what its provider produced. `SpawnMode::Session` is what keeps the session
/// alive across turns; `Run` (the default) ends after the first message.
#[tokio::test]
async fn a_spawned_subagent_runs_turns_in_its_session() {
    install_test_root();

    let acp = AcpControlPlane::new(50)
        .with_agent_builder(scripted_builder(vec!["first-answer", "second-answer"]));

    let session_id = acp.create_session("parent".to_string()).await;
    let handle = acp
        .spawn_subagent(
            session_id.clone(),
            "parent".to_string(),
            SubagentConfig {
                mode: SpawnMode::Session,
                thread_binding: ThreadBinding::New,
                ..Default::default()
            },
        )
        .await
        .expect("spawn must succeed once a builder is installed");

    let first = acp
        .send_message(&handle.id, IncomingMessage::new("user", "conv-1", "first"))
        .await
        .expect("first turn");
    assert_eq!(first, "first-answer");

    let second = acp
        .send_message(&handle.id, IncomingMessage::new("user", "conv-2", "second"))
        .await
        .expect("a Session-mode sub-agent must accept a second turn");
    assert_eq!(second, "second-answer");

    // The session lists the sub-agent it spawned.
    let listed = acp.list_session_subagents(&session_id).await;
    assert!(
        listed.iter().any(|s| s.id == handle.id),
        "the session must list its sub-agent, got {:?}",
        listed.iter().map(|s| &s.id).collect::<Vec<_>>()
    );

    assert!(acp.shutdown_subagent(&handle.id).await.expect("shutdown"));
    acp.shutdown().await.expect("control plane shutdown");
}

/// Case 2 — a `Run`-mode sub-agent is one-shot: after its single turn the
/// command channel closes, and a further message is refused rather than
/// silently dropped.
#[tokio::test]
async fn a_run_mode_subagent_is_one_shot() {
    install_test_root();

    let acp = AcpControlPlane::new(50).with_agent_builder(scripted_builder(vec!["only-answer"]));

    let session_id = acp.create_session("parent".to_string()).await;
    let handle = acp
        .spawn_subagent(
            session_id,
            "parent".to_string(),
            SubagentConfig {
                mode: SpawnMode::Run,
                thread_binding: ThreadBinding::New,
                ..Default::default()
            },
        )
        .await
        .expect("spawn");

    let answer = acp
        .send_message(&handle.id, IncomingMessage::new("user", "conv", "go"))
        .await
        .expect("the single run-mode turn");
    assert_eq!(answer, "only-answer");

    let second = acp
        .send_message(&handle.id, IncomingMessage::new("user", "conv", "again"))
        .await;
    assert!(
        second.is_err(),
        "a Run-mode sub-agent must not accept a second turn, got {second:?}"
    );

    acp.shutdown().await.expect("shutdown");
}
