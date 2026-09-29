//! HTTP-surface e2e tests.
//!
//! Every other e2e test in this tree speaks WebSocket. The gateway's HTTP
//! surface — the probes, the OpenAI-compatible endpoints, the upgrade-ticket
//! exchange, and the separately-routed webhooks — had no end-to-end
//! coverage at all; correctness lived only in in-file unit tests that never
//! crossed a socket. These tests drive the real router over TCP.

use super::*;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::RwLock;

/// Base URL for a gateway started on `port`.
fn base(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// Start a gateway with the mock provider and HTTP-friendly config.
async fn start_http_gateway(port: u16, mock: MockProvider, extra: impl FnOnce(&mut GatewayConfig)) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    extra(&mut config);

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock, "mock-model").await;
    start_gateway_and_wait_opts(port, gateway, true).await;
}

/// Case 1 — the probes and metrics endpoints answer over real HTTP.
#[tokio::test]
#[serial]
async fn probes_and_metrics_are_served_over_http() {
    let port = free_port();
    start_http_gateway(port, llm_mock_provider_for_streaming(), |_| {}).await;

    for (path, expect_body) in [
        ("/live", None),
        ("/health", None),
        ("/metrics", Some("# ")), // Prometheus text exposition starts with comments
    ] {
        let resp = reqwest::get(format!("{}{}", base(port), path))
            .await
            .unwrap_or_else(|e| panic!("GET {path} failed: {e}"));
        assert_eq!(resp.status(), 200, "GET {path} must be 200");
        if let Some(marker) = expect_body {
            let body = resp.text().await.expect("body");
            assert!(body.contains(marker), "GET {path} body missing {marker:?}: {body:.120}");
        }
    }

    // `/ready` is a readiness gate, not a liveness one: with no channels
    // registered the gateway reports itself not ready (503) and names which
    // component is missing. That contract is what a deploy's health check
    // depends on, so assert the shape, not an aspirational 200.
    let resp = reqwest::get(format!("{}/ready", base(port)))
        .await
        .expect("GET /ready");
    assert_eq!(resp.status(), 503, "no channels registered → not ready");
    let body: serde_json::Value = resp.json().await.expect("ready JSON");
    assert_eq!(body["ready"], false);
    assert_eq!(body["channels"]["count"], 0);
    assert!(body["agents"]["ready"].as_bool().unwrap_or(false), "default agent is up");
}

/// Case 2 — the OpenAI-compatible model list reflects the registered catalog.
#[tokio::test]
#[serial]
async fn openai_models_endpoint_lists_registered_models() {
    let port = free_port();
    start_http_gateway(port, llm_mock_provider_for_streaming(), |_| {}).await;

    let body: serde_json::Value = reqwest::get(format!("{}/v1/models", base(port)))
        .await
        .expect("GET /v1/models")
        .json()
        .await
        .expect("JSON body");

    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        ids.iter().any(|id| id.contains("mock")),
        "the registered mock model must appear; got {ids:?}"
    );
}

/// Case 3 — `/v1/chat/completions` completes through a real provider call.
#[tokio::test]
#[serial]
async fn openai_chat_completions_answers_with_the_provider_response() {
    let port = free_port();
    start_http_gateway(port, llm_mock_provider_for_streaming(), |_| {}).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", base(port)))
        .json(&serde_json::json!({
            "model": "mock-model",
            "messages": [{ "role": "user", "content": "Say exactly 'pong-from-llm'." }],
            "stream": false
        }))
        .send()
        .await
        .expect("POST /v1/chat/completions");

    assert_eq!(resp.status(), 200, "completions must answer 200");
    let body: serde_json::Value = resp.json().await.expect("JSON body");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(
        content.contains("pong-from-llm"),
        "the provider's answer must ride the OpenAI wire shape; got {body}"
    );
}

/// Case 4 — a ticket minted over HTTP authenticates a WebSocket upgrade.
///
/// This is the documented reason the REST endpoint exists (a browser cannot
/// set headers on an upgrade), so the journey must cross both surfaces.
#[tokio::test]
#[serial]
async fn ws_ticket_minted_over_http_authenticates_a_websocket() {
    let port = free_port();
    start_http_gateway(port, llm_mock_provider_for_streaming(), |_| {}).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/api/v1/ws-ticket", base(port)))
        .send()
        .await
        .expect("POST /api/v1/ws-ticket");
    assert_eq!(resp.status(), 200, "ticket minting must answer 200");
    let body: serde_json::Value = resp.json().await.expect("ticket JSON body");
    let ticket = body["ticket"].as_str().expect("ticket field").to_string();
    assert!(!ticket.is_empty(), "ticket must be non-empty");

    // The frontend simulator's plain connect path proves the socket answers;
    // the ticket path is what this test adds. A token-authed gateway would
    // reject an unticketed upgrade — here auth is `none`, so instead assert
    // the ticket-bearing upgrade also completes and can open a session.
    let mut client = FrontendSimulator::connect_with_ticket(port, &ticket).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Say exactly 'pong-from-llm' and nothing else.")
        .await;
    let payload = client
        .wait_for_event("chat.final", 30)
        .await
        .expect("turn must complete over the ticketed socket");
    assert!(payload.get("response").is_some(), "chat.final carries a response");
}

/// A channel adapter that records what the gateway sends back.
struct HttpTestChannel {
    name: String,
    sent: Arc<RwLock<Vec<syscity::channels::OutgoingMessage>>>,
}

impl HttpTestChannel {
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
impl syscity::channels::Channel for HttpTestChannel {
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

/// Compute the Slack `v0` signature the gateway verifies.
fn slack_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(format!("v0:{}:", timestamp).as_bytes());
    mac.update(body);
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

/// Case 5 — a signed Slack webhook drives the full channel round trip:
/// signature verification → access control → inbound pipeline → agent →
/// reply dispatched back to the same channel.
#[tokio::test]
#[serial]
async fn slack_webhook_round_trip_replies_to_the_channel() {
    const SECRET: &str = "test-signing-secret";
    let port = free_port();

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.channels.insert("slack".to_string(), {
        let mut c =
            syscity::gateway::config::ChannelConfig::new(syscity::channels::ChannelType::Slack);
        c.credentials
            .insert("signing_secret".to_string(), SECRET.to_string());
        c
    });

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    // Register the recording adapter before start, under the name the webhook
    // provenance carries ("slack").
    let state = gateway.state();
    let (channel, sent) = HttpTestChannel::new("slack");
    state
        .channels
        .reply_dispatcher
        .register_channel("slack", channel)
        .await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    start_gateway_and_wait_opts(port, gateway, true).await;

    let body = serde_json::to_vec(&serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": "U_HTTP_E2E",
            "channel": "D_HTTP_E2E",
            "text": "Say exactly 'pong-from-llm' and nothing else."
        }
    }))
    .expect("serialize");
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let signature = slack_signature(SECRET, &timestamp, &body);

    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/slack", base(port)))
        .header("x-slack-request-timestamp", &timestamp)
        .header("x-slack-signature", &signature)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST /webhooks/slack");
    assert_eq!(resp.status(), 200, "a signed webhook must be accepted");

    // The reply must come out of the same channel.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("timed out waiting for the webhook reply on channel 'slack'");
        }
        let snapshot = sent.read().await.clone();
        if snapshot.iter().any(|m| m.content.contains("pong-from-llm")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 6 — an unsigned (or wrongly signed) webhook is refused. The endpoint
/// is in the essential tier precisely because its authentication is the
/// per-channel signature; a regression here would be an open ingress.
#[tokio::test]
#[serial]
async fn slack_webhook_rejects_a_bad_signature() {
    const SECRET: &str = "test-signing-secret";
    let port = free_port();

    start_http_gateway(port, llm_mock_provider_for_streaming(), |config| {
        config.channels.insert("slack".to_string(), {
            let mut c =
                syscity::gateway::config::ChannelConfig::new(syscity::channels::ChannelType::Slack);
            c.credentials
                .insert("signing_secret".to_string(), SECRET.to_string());
            c
        });
    })
    .await;

    let body = serde_json::to_vec(&serde_json::json!({
        "type": "event_callback",
        "event": { "type": "message", "user": "U", "channel": "D", "text": "hi" }
    }))
    .expect("serialize");
    let timestamp = chrono::Utc::now().timestamp().to_string();

    for (label, sig) in [
        ("wrong secret", slack_signature("not-the-secret", &timestamp, &body)),
        ("garbage", "v0=deadbeef".to_string()),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{}/webhooks/slack", base(port)))
            .header("x-slack-request-timestamp", &timestamp)
            .header("x-slack-signature", &sig)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
            .expect("POST /webhooks/slack");
        assert_eq!(
            resp.status(),
            401,
            "{label}: a webhook that fails signature verification must be refused"
        );
    }
}
