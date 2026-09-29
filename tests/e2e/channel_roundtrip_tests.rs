//! Channel round-trip journey tests.
//!
//! The WS-side chat journeys (`llm_chat_tests`) cover prompt → agent →
//! reply → persisted history over the WebSocket. This file covers the other
//! front door: a message arriving over a **chat channel** must come back as
//! a reply on the **same channel** —
//!
//! ```text
//! IncomingMessage (provenance: ExternalUser { channel })
//!   → pipelines.inbound_entry → inbound pipeline (debounce → route)
//!   → routed dispatch → agent turn (mock provider)
//!   → OutboundContext { channel } → outbound pipeline
//!   → DispatchStage → reply_dispatcher → channel.send()
//! ```
//!
//! No real channel platform is involved: the test registers a `TestChannel`
//! into the gateway's reply dispatcher and asserts the reply lands in its
//! send log.

use super::*;

use async_trait::async_trait;
use syscity::channels::{Channel, ChannelCapabilities, ConversationId, OutgoingMessage};
use syscity::core::models::Id;
use tokio::sync::RwLock;

/// A minimal in-memory channel that records every `send` it receives.
struct TestChannel {
    name: String,
    sent: Arc<RwLock<Vec<OutgoingMessage>>>,
}

impl TestChannel {
    fn new(name: impl Into<String>) -> (Arc<Self>, Arc<RwLock<Vec<OutgoingMessage>>>) {
        let sent = Arc::new(RwLock::new(Vec::new()));
        (
            Arc::new(Self {
                name: name.into(),
                sent: sent.clone(),
            }),
            sent,
        )
    }
}

#[async_trait]
impl Channel for TestChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }

    async fn start(&self) -> syscity::Result<()> {
        Ok(())
    }

    async fn stop(&self) -> syscity::Result<()> {
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> syscity::Result<Id> {
        self.sent.write().await.push(message);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> syscity::Result<()> {
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> syscity::Result<()> {
        Ok(())
    }

    async fn delete_message(&self, _message_id: Id) -> syscity::Result<()> {
        Ok(())
    }

    async fn health_check(&self) -> syscity::Result<bool> {
        Ok(true)
    }
}

/// Drive a channel message through the full round trip and assert the reply
/// reaches the channel.
#[tokio::test]
#[serial]
async fn channel_message_round_trip_reaches_the_channel() {
    let port = free_port();

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = syscity::gateway::Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");

    // Grab the state handle before `start_gateway_and_wait` moves the
    // gateway into its runtime task.
    let state = gateway.state();

    let mock = llm_mock_provider_for_streaming();
    register_mock_provider_with_model(&gateway.model_router(), mock, "mock-model").await;

    let (channel, sent) = TestChannel::new("fakechat");
    state
        .channels
        .reply_dispatcher
        .register_channel("fakechat", channel)
        .await;

    start_gateway_and_wait(port, gateway).await;

    // A message from a user over the channel. Provenance names the channel,
    // which is how the outbound side knows where to reply.
    let incoming = syscity::channels::IncomingMessage::new(
        "ch-user-1",
        "fakechat-conv-1",
        "Say exactly 'pong-from-llm' and nothing else.",
    )
    .with_provenance(syscity::channels::InputProvenance::ExternalUser {
        channel: "fakechat".to_string(),
        is_direct: true,
    });

    state
        .pipelines
        .inbound_entry
        .send(incoming)
        .await
        .expect("inbound entry channel must be open after gateway start");

    // The pipeline debounces (~500 ms default) and the agent turn runs
    // asynchronously; poll the channel's send log for the reply.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let reply = loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("Timed out waiting for the reply on channel 'fakechat'");
        }
        let snapshot = sent.read().await.clone();
        if let Some(msg) = snapshot
            .iter()
            .find(|m| m.content.contains("pong-from-llm"))
        {
            break msg.content.clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    assert!(reply.contains("pong-from-llm"), "reply: {reply}");
}

/// The reply that reaches the channel must be the *assistant's* answer —
/// an echo of the user's own prompt is not a reply. This pins the direction:
/// inbound content must not be dispatched back out verbatim.
#[tokio::test]
#[serial]
async fn channel_round_trip_sends_the_assistant_answer_not_the_prompt() {
    let port = free_port();

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = syscity::gateway::Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let state = gateway.state();

    let mock = llm_mock_provider_for_streaming();
    register_mock_provider_with_model(&gateway.model_router(), mock, "mock-model").await;

    let (channel, sent) = TestChannel::new("fakechat2");
    state
        .channels
        .reply_dispatcher
        .register_channel("fakechat2", channel)
        .await;

    start_gateway_and_wait(port, gateway).await;

    let incoming = syscity::channels::IncomingMessage::new(
        "ch-user-2",
        "fakechat-conv-2",
        "unique-prompt-marker-7731",
    )
    .with_provenance(syscity::channels::InputProvenance::ExternalUser {
        channel: "fakechat2".to_string(),
        is_direct: true,
    });

    state
        .pipelines
        .inbound_entry
        .send(incoming)
        .await
        .expect("inbound entry channel must be open after gateway start");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("Timed out waiting for the reply on channel 'fakechat2'");
        }
        let snapshot = sent.read().await.clone();
        if snapshot.iter().any(|m| m.content.contains("pong-from-llm")) {
            // The reply arrived. None of the sends may carry the raw prompt.
            for m in &snapshot {
                assert!(
                    !m.content.contains("unique-prompt-marker-7731"),
                    "the user's prompt was echoed back as a reply: {}",
                    m.content
                );
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
