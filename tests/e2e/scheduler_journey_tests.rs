//! Heartbeat and standing-order journeys.
//!
//! Both schedulers were covered only by their own scheduling arithmetic
//! (`heartbeat/runner.rs` 9 tests, `standing_orders/mod.rs` 10), and neither
//! had a test proving the chain the user cares about: the scheduler fires, the
//! agent runs the configured prompt, and the result goes where the config said
//! it should. A scheduler that computes the right next-instant but never
//! reaches an agent looks perfectly tested.
//!
//! Wall-clock care: the heartbeat's default active window is 08:00–23:00, so a
//! test that inherits it would skip outside those hours. These tests pin a
//! 24-hour window and drive the wake manually.

use super::*;

use tokio::sync::RwLock;

use syscity::heartbeat::{HeartbeatConfig, WakePriority, WakeRequest};
use syscity::standing_orders::config::{StandingOrderConfig, StandingOrderDef};

/// A recording channel for the standing-order dispatch leg.
struct SchedTestChannel {
    name: String,
    sent: Arc<RwLock<Vec<syscity::channels::OutgoingMessage>>>,
}

impl SchedTestChannel {
    fn new(
        name: impl Into<String>,
    ) -> (Arc<Self>, Arc<RwLock<Vec<syscity::channels::OutgoingMessage>>>) {
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

#[async_trait::async_trait]
impl syscity::channels::Channel for SchedTestChannel {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> syscity::channels::ChannelCapabilities {
        syscity::channels::ChannelCapabilities::default()
    }
    async fn start(&self) -> syscity::Result<()> {
        Ok(())
    }
    async fn stop(&self) -> syscity::Result<()> {
        Ok(())
    }
    async fn send(
        &self,
        message: syscity::channels::OutgoingMessage,
    ) -> syscity::Result<syscity::core::models::Id> {
        self.sent.write().await.push(message);
        Ok(syscity::core::models::Id::new())
    }
    async fn send_typing(&self, _c: &syscity::channels::ConversationId) -> syscity::Result<()> {
        Ok(())
    }
    async fn edit_message(
        &self,
        _id: syscity::core::models::Id,
        _new: String,
    ) -> syscity::Result<()> {
        Ok(())
    }
    async fn delete_message(&self, _id: syscity::core::models::Id) -> syscity::Result<()> {
        Ok(())
    }
    async fn health_check(&self) -> syscity::Result<bool> {
        Ok(true)
    }
}

/// A 24-hour heartbeat window with a long interval: the interval must not fire
/// during the test, and the active-hours gate must not depend on the wall
/// clock.
fn always_on_heartbeat() -> HeartbeatConfig {
    HeartbeatConfig {
        enabled: true,
        interval_seconds: 3600,
        active_hours_start: "00:00".to_string(),
        active_hours_end: "23:59".to_string(),
        ..Default::default()
    }
}

/// Wait for the mock provider to record a request containing `marker`.
async fn wait_for_model_prompt(mock: &MockProvider, marker: &str, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: the prompt never reached the model"
        );
        if mock
            .history()
            .iter()
            .any(|req| req.messages.iter().any(|m| m.content.contains(marker)))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 1 — a wake request reaches the agent: the custom prompt lands in a
/// model request. This is the "heartbeat fires → agent works" half that the
/// scheduling unit tests never crossed.
#[tokio::test]
#[serial]
async fn heartbeat_wake_runs_the_agent_with_the_custom_prompt() {
    const MARKER: &str = "HEARTBEAT-MARKER-7788";

    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.heartbeat = always_on_heartbeat();

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let state = gateway.state();
    let mock = llm_mock_provider_for_streaming();
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock.clone(), "mock-model").await;
    start_gateway_and_wait_opts(port, gateway, true).await;

    let wake_tx = state
        .scheduler
        .heartbeat_wake_tx
        .read()
        .await
        .clone()
        .expect("the gateway installs a heartbeat wake sender");
    wake_tx
        .send(WakeRequest {
            agent_id: "default".to_string(),
            priority: WakePriority::Default,
            prompt: Some(format!("Report the phrase {MARKER}.")),
        })
        .await
        .expect("wake request accepted");

    wait_for_model_prompt(&mock, MARKER, "heartbeat wake").await;
}

/// Case 2 — with heartbeat disabled the runner is not started at all:
/// there is no wake channel to send into and no event stream to observe.
/// The gate lives at startup (`lifecycle.rs` spawns the runner only when
/// `config.heartbeat.enabled`), so "disabled" means absent, not dormant.
#[tokio::test]
#[serial]
async fn heartbeat_runner_is_absent_when_disabled() {
    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.heartbeat = HeartbeatConfig {
        enabled: false,
        ..always_on_heartbeat()
    };

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let state = gateway.state();
    let mock = llm_mock_provider_for_streaming();
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock.clone(), "mock-model").await;
    start_gateway_and_wait_opts(port, gateway, true).await;

    assert!(
        state.scheduler.heartbeat_wake_tx.read().await.is_none(),
        "a disabled heartbeat must not install a wake channel"
    );
    assert!(
        state.scheduler.heartbeat_event_tx.read().await.is_none(),
        "a disabled heartbeat must not install an event stream"
    );

    // And nothing in the scheduler ran the agent on its own.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        mock.history().is_empty(),
        "a disabled heartbeat must not drive the agent; saw {} request(s)",
        mock.history().len()
    );
}

/// Case 3 — a standing order fires on its schedule, the agent runs the
/// configured prompt, and the reply is dispatched to the configured channel.
/// The schedule is six-field, so `*/1 * * * * *` fires within a second.
#[tokio::test]
#[serial]
async fn standing_order_prompt_reaches_the_agent_and_its_reply_the_channel() {
    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.standing_orders = StandingOrderConfig {
        enabled: true,
        orders: vec![StandingOrderDef {
            name: "journey-order".to_string(),
            description: None,
            agent_id: "default".to_string(),
            schedule: "*/1 * * * * *".to_string(),
            prompt: "Say exactly 'pong-from-llm' and nothing else.".to_string(),
            output_channel: Some("schedchan".to_string()),
            enabled: true,
            timeout_secs: Some(60),
        }],
    };

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let state = gateway.state();
    let (channel, sent) = SchedTestChannel::new("schedchan");
    state
        .channels
        .reply_dispatcher
        .register_channel("schedchan", channel)
        .await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;
    start_gateway_and_wait_opts(port, gateway, true).await;

    // The order dispatches the agent's reply to the channel it names.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("the standing order's reply never reached channel 'schedchan'");
        }
        let snapshot = sent.read().await.clone();
        if snapshot.iter().any(|m| m.content.contains("pong-from-llm")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
