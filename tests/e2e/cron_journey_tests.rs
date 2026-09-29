//! Cron scheduler journey tests.
//!
//! The scheduler's logic is unit-tested in depth (`src/cron/cron/mod.rs`, 44
//! tests), but nothing proved the chain end to end: a job firing, its prompt
//! reaching an agent, and its result arriving where the job said to put it.
//! The announce half was in fact broken — `DeliveryMode::Announce` produced
//! only a WS event and no channel ever consumed it, so a job addressed at a
//! channel reached nobody. These tests pin both halves.
//!
//! Jobs are triggered with `trigger_job` (the `force` path) rather than
//! waiting on a wall-clock tick, so the tests are deterministic.

use super::*;

use tokio::sync::RwLock;

use syscity::cron::cron::{
    CronJob, CronScheduler, DeliveryMode, ExecutionTarget, Schedule, SessionTarget,
};

/// A recording channel, same shape as the one in `channel_roundtrip_tests`.
struct CronTestChannel {
    name: String,
    sent: Arc<RwLock<Vec<syscity::channels::OutgoingMessage>>>,
}

impl CronTestChannel {
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
impl syscity::channels::Channel for CronTestChannel {
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

/// Fail loudly if the gateway never produced a cron scheduler.
async fn scheduler(
    state: &Arc<syscity::gateway::GatewayState>,
) -> Arc<tokio::sync::Mutex<CronScheduler>> {
    let guard = state.scheduler.cron_scheduler.read().await;
    guard
        .clone()
        .expect("gateway start must install the cron scheduler")
}

/// Case 1 — a triggered agent-target job's prompt reaches the model, proving
/// the scheduler → agent chain (previously covered only by unit tests of the
/// scheduling arithmetic).
#[tokio::test]
#[serial]
async fn triggered_cron_job_runs_its_prompt_through_the_agent() {
    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let state = gateway.state();
    let mock = llm_mock_provider_for_streaming();
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock.clone(), "mock-model").await;

    start_gateway_and_wait_opts(port, gateway, true).await;

    let sched = scheduler(&state).await;
    let sched = sched.lock().await;
    let mut job = CronJob::new(
        "journey-agent",
        "journey agent job",
        Schedule::Cron {
            expression: "0 0 1 1 *".to_string(),
            timezone: None,
            stagger_ms: None,
        },
        ExecutionTarget::agent("Say exactly 'pong-from-llm' and nothing else."),
    );
    job.session = SessionTarget::Isolated;
    job.delivery = DeliveryMode::None;
    sched.add_job(job).await.expect("add job");
    sched
        .trigger_job("journey-agent")
        .await
        .expect("trigger job");
    drop(sched);

    // The agent ran when the mock provider recorded a completion carrying the
    // job's prompt.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the cron job's prompt never reached the model"
        );
        let reached = mock.history().iter().any(|req| {
            req.messages
                .iter()
                .any(|m| m.content.contains("pong-from-llm"))
        });
        if reached {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 2 — `DeliveryMode::Announce` reaches the named channel. This was the
/// broken half: the announce became a WS event with no channel consumer.
#[tokio::test]
#[serial]
async fn cron_announce_delivers_to_the_named_channel() {
    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let state = gateway.state();
    let (channel, sent) = CronTestChannel::new("fakecron");
    state
        .channels
        .reply_dispatcher
        .register_channel("fakecron", channel)
        .await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    start_gateway_and_wait_opts(port, gateway, true).await;

    let sched = scheduler(&state).await;
    let sched = sched.lock().await;
    let mut job = CronJob::new(
        "journey-announce",
        "journey announce job",
        Schedule::Cron {
            expression: "0 0 1 1 *".to_string(),
            timezone: None,
            stagger_ms: None,
        },
        ExecutionTarget::agent("Say exactly 'pong-from-llm' and nothing else."),
    );
    job.session = SessionTarget::Isolated;
    job.delivery = DeliveryMode::Announce {
        channel: "fakecron".to_string(),
        to: "room-42".to_string(),
    };
    sched.add_job(job).await.expect("add job");
    sched
        .trigger_job("journey-announce")
        .await
        .expect("trigger job");
    drop(sched);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("the cron announce never reached channel 'fakecron'");
        }
        let snapshot = sent.read().await.clone();
        if let Some(msg) = snapshot
            .iter()
            .find(|m| m.content.contains("pong-from-llm"))
        {
            assert_eq!(
                msg.conversation_id.0, "room-42",
                "the announce must go to the recipient the job named"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
