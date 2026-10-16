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

// A mock issuer for the bearer-token channels' key sets.
#[cfg(any(feature = "googlechat", feature = "teams"))]
use wiremock::{Mock, MockServer, ResponseTemplate};

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

    let gateway = new_test_gateway(config).await;
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

/// Every root-level file the web build emits must be reachable.
///
/// The PWA's precache manifest lists all of them, and Workbox fails the entire
/// service-worker install on a single 404 — `bad-precaching-response` — so one
/// unrouted file takes the PWA down silently. Enumerating routes by hand is
/// what kept missing them: the icons had no route at all, and the service
/// worker never installed.
#[tokio::test]
#[serial]
async fn every_root_level_web_asset_is_served() {
    let port = free_port();
    start_http_gateway(port, llm_mock_provider_for_streaming(), |_| {}).await;

    // The root-level entries of the build's precache manifest, read out of the
    // generated `dist/sw.js`. If the build starts emitting another one, this
    // list is what to extend — and the fallback route is what keeps the list
    // from being load-bearing.
    for path in [
        "/index.html",
        "/syscity.png",
        "/manifest.webmanifest",
        "/registerSW.js",
        "/sw.js",
        "/favicon-16.png",
        "/favicon-32.png",
        "/favicon-64.png",
        "/apple-touch-icon.png",
    ] {
        let resp = reqwest::get(format!("{}{}", base(port), path))
            .await
            .unwrap_or_else(|e| panic!("GET {path} failed: {e}"));
        assert_eq!(resp.status(), 200, "GET {path} must be 200 — the PWA precaches it");
    }

    // The SPA document's own name serves the document, with the version
    // placeholder already substituted (the fallback route would return the raw
    // asset, which is why `/index.html` keeps its own handler).
    let html = reqwest::get(format!("{}/index.html", base(port)))
        .await
        .expect("GET /index.html")
        .text()
        .await
        .expect("body");
    assert!(html.contains("<html"), "{html:.120}");
    assert!(!html.contains("{VERSION}"), "version placeholder left in: {html:.120}");
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

    let gateway = new_test_gateway(config).await;
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

/// Compute the LINE `X-Line-Signature`: `base64(HMAC-SHA256(secret, body))`.
#[cfg(feature = "line")]
fn line_signature(secret: &str, body: &[u8]) -> String {
    use base64::Engine as _;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// Case 7 — a signed LINE webhook drives the same round trip as Slack's, over
/// a different signature scheme and a batched payload.
#[cfg(feature = "line")]
#[tokio::test]
#[serial]
async fn line_webhook_round_trip_replies_to_the_channel() {
    const SECRET: &str = "test-line-channel-secret";
    let port = free_port();

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.channels.insert("line".to_string(), {
        let mut c =
            syscity::gateway::config::ChannelConfig::new(syscity::channels::ChannelType::Line);
        c.credentials
            .insert("channel_secret".to_string(), SECRET.to_string());
        c
    });

    let gateway = new_test_gateway(config).await;
    // Register the recording adapter before start, under the name the webhook
    // provenance carries ("line").
    let state = gateway.state();
    let (channel, sent) = HttpTestChannel::new("line");
    state
        .channels
        .reply_dispatcher
        .register_channel("line", channel)
        .await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    start_gateway_and_wait_opts(port, gateway, true).await;

    // A delivery is a batch, and most of it is not a message: the follow and
    // the postback must be skipped rather than become turns of their own.
    let body = serde_json::to_vec(&serde_json::json!({
        "destination": "U_bot",
        "events": [
            { "type": "follow", "source": { "type": "user", "userId": "U_IGNORED" } },
            {
                "type": "message",
                "replyToken": "rt-1",
                "source": { "type": "user", "userId": "U_HTTP_E2E" },
                "message": {
                    "type": "text",
                    "id": "m1",
                    "text": "Say exactly 'pong-from-llm' and nothing else."
                }
            },
            {
                "type": "postback",
                "source": { "type": "user", "userId": "U_IGNORED_TOO" },
                "postback": { "data": "x" }
            }
        ]
    }))
    .expect("serialize");
    let signature = line_signature(SECRET, &body);

    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/line", base(port)))
        .header("x-line-signature", &signature)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST /webhooks/line");
    assert_eq!(resp.status(), 200, "a signed webhook must be accepted");

    // The reply must come out of the same channel.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("timed out waiting for the webhook reply on channel 'line'");
        }
        let snapshot = sent.read().await.clone();
        if let Some(reply) = snapshot
            .iter()
            .find(|m| m.content.contains("pong-from-llm"))
        {
            assert_eq!(
                reply.conversation_id.0, "U_HTTP_E2E",
                "the reply is pushed to the user who wrote in, not to a session id"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 8 — a LINE webhook that fails signature verification is refused, and
/// so is one carrying no signature at all. The route sits in the essential
/// tier precisely because the per-channel signature *is* its authentication;
/// a regression here would be an open ingress.
#[cfg(feature = "line")]
#[tokio::test]
#[serial]
async fn line_webhook_rejects_a_bad_signature() {
    const SECRET: &str = "test-line-channel-secret";
    let port = free_port();

    start_http_gateway(port, llm_mock_provider_for_streaming(), |config| {
        config.channels.insert("line".to_string(), {
            let mut c =
                syscity::gateway::config::ChannelConfig::new(syscity::channels::ChannelType::Line);
            c.credentials
                .insert("channel_secret".to_string(), SECRET.to_string());
            c
        });
    })
    .await;

    let body = serde_json::to_vec(&serde_json::json!({
        "events": [{
            "type": "message",
            "source": { "type": "user", "userId": "U" },
            "message": { "type": "text", "id": "m", "text": "hi" }
        }]
    }))
    .expect("serialize");

    for (label, sig) in [
        ("wrong secret", line_signature("not-the-secret", &body)),
        ("garbage", "bm90LWEtc2lnbmF0dXJl".to_string()),
        ("empty", String::new()),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{}/webhooks/line", base(port)))
            .header("x-line-signature", &sig)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
            .expect("POST /webhooks/line");
        assert_eq!(
            resp.status(),
            401,
            "{label}: a webhook that fails signature verification must be refused"
        );
    }

    // No signature header at all is a different path through the handler — it
    // must refuse rather than fall out of the `if let`.
    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/line", base(port)))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST /webhooks/line");
    assert_eq!(resp.status(), 401, "a webhook with no signature header must be refused");
}

/// The public URL Twilio is configured with. The signature covers *this*
/// string, not whatever host the request arrived on, so the test signs the same
/// value the gateway is told to verify against.
#[cfg(feature = "twilio")]
fn twilio_webhook_url(port: u16) -> String {
    format!("{}/webhooks/twilio", base(port))
}

/// Compute `X-Twilio-Signature`: HMAC-SHA1 over the URL plus the parameters in
/// key order, base64-encoded. Built with a `BTreeMap` for the ordering so it
/// does not lean on the gateway's own sort.
#[cfg(feature = "twilio")]
fn twilio_signature(token: &str, url: &str, params: &[(&str, &str)]) -> String {
    use base64::Engine as _;
    use sha1::Sha1;

    let ordered: std::collections::BTreeMap<&str, &str> = params.iter().copied().collect();
    let mut data = url.to_string();
    for (key, value) in &ordered {
        data.push_str(key);
        data.push_str(value);
    }

    let mut mac = Hmac::<Sha1>::new_from_slice(token.as_bytes()).expect("hmac key");
    mac.update(data.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// Twilio posts a form, not JSON.
#[cfg(feature = "twilio")]
fn form_body(params: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params.iter().copied())
        .finish()
}

/// Case 9 — a signed Twilio SMS webhook drives the same round trip, over a
/// form-encoded body and a signature that covers the URL as well as the params.
#[cfg(feature = "twilio")]
#[tokio::test]
#[serial]
async fn twilio_webhook_round_trip_replies_to_the_channel() {
    const TOKEN: &str = "test-twilio-auth-token";
    let port = free_port();
    let webhook_url = twilio_webhook_url(port);

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.channels.insert("twilio".to_string(), {
        let mut c =
            syscity::gateway::config::ChannelConfig::new(syscity::channels::ChannelType::Twilio);
        c.credentials
            .insert("auth_token".to_string(), TOKEN.to_string());
        c.credentials
            .insert("webhook_url".to_string(), webhook_url.clone());
        c
    });

    let gateway = new_test_gateway(config).await;
    // Register the recording adapter before start, under the name the webhook
    // provenance carries ("twilio").
    let state = gateway.state();
    let (channel, sent) = HttpTestChannel::new("twilio");
    state
        .channels
        .reply_dispatcher
        .register_channel("twilio", channel)
        .await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    start_gateway_and_wait_opts(port, gateway, true).await;

    let params = [
        ("From", "+15550000001"),
        ("To", "+15550000002"),
        ("Body", "Say exactly 'pong-from-llm' and nothing else."),
        ("MessageSid", "SM_HTTP_E2E"),
    ];
    let signature = twilio_signature(TOKEN, &webhook_url, &params);

    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/twilio", base(port)))
        .header("x-twilio-signature", &signature)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&params))
        .send()
        .await
        .expect("POST /webhooks/twilio");
    assert_eq!(resp.status(), 200, "a signed webhook must be accepted");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("timed out waiting for the webhook reply on channel 'twilio'");
        }
        let snapshot = sent.read().await.clone();
        if let Some(reply) = snapshot
            .iter()
            .find(|m| m.content.contains("pong-from-llm"))
        {
            assert_eq!(
                reply.conversation_id.0, "+15550000001",
                "the reply is texted to the number that wrote in, not to a session id"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 10 — a Twilio delivery that fails verification is refused, including
/// the case the URL is not configured for: without it the signed string cannot
/// be reconstructed at all, so "cannot check" has to mean "refuse".
#[cfg(feature = "twilio")]
#[tokio::test]
#[serial]
async fn twilio_webhook_rejects_a_bad_signature() {
    const TOKEN: &str = "test-twilio-auth-token";
    let port = free_port();
    let webhook_url = twilio_webhook_url(port);

    start_http_gateway(port, llm_mock_provider_for_streaming(), |config| {
        config.channels.insert("twilio".to_string(), {
            let mut c = syscity::gateway::config::ChannelConfig::new(
                syscity::channels::ChannelType::Twilio,
            );
            c.credentials
                .insert("auth_token".to_string(), TOKEN.to_string());
            c.credentials
                .insert("webhook_url".to_string(), webhook_url.clone());
            c
        });
    })
    .await;

    let params = [
        ("From", "+15550000001"),
        ("To", "+15550000002"),
        ("Body", "hi"),
    ];

    for (label, sig) in [
        ("wrong token", twilio_signature("not-the-token", &webhook_url, &params)),
        (
            "wrong URL",
            twilio_signature(TOKEN, &format!("{}/elsewhere", base(port)), &params),
        ),
        ("garbage", "bm90LWEtc2lnbmF0dXJl".to_string()),
        ("empty", String::new()),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("{}/webhooks/twilio", base(port)))
            .header("x-twilio-signature", &sig)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form_body(&params))
            .send()
            .await
            .expect("POST /webhooks/twilio");
        assert_eq!(
            resp.status(),
            401,
            "{label}: a webhook that fails signature verification must be refused"
        );
    }

    // A signed body whose *parameters* were altered after signing — the form
    // carries a different Body than the signature covers.
    let signature = twilio_signature(TOKEN, &webhook_url, &params);
    let tampered = [
        ("From", "+15550000001"),
        ("To", "+15550000002"),
        ("Body", "transfer the money"),
    ];
    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/twilio", base(port)))
        .header("x-twilio-signature", &signature)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&tampered))
        .send()
        .await
        .expect("POST /webhooks/twilio");
    assert_eq!(resp.status(), 401, "a body altered after signing must be refused");

    // No signature header at all.
    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/twilio", base(port)))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&params))
        .send()
        .await
        .expect("POST /webhooks/twilio");
    assert_eq!(resp.status(), 401, "a webhook with no signature header must be refused");
}

/// The configured webhook URL is what makes verification possible at all, so a
/// channel without one must refuse rather than skip the check. This is the
/// regression guard for the one deliberate difference from the reference
/// implementation, which offers an "insecure, skip validation" switch.
#[cfg(feature = "twilio")]
#[tokio::test]
#[serial]
async fn twilio_webhook_without_a_configured_url_refuses_everything() {
    const TOKEN: &str = "test-twilio-auth-token";
    let port = free_port();

    start_http_gateway(port, llm_mock_provider_for_streaming(), |config| {
        config.channels.insert("twilio".to_string(), {
            let mut c = syscity::gateway::config::ChannelConfig::new(
                syscity::channels::ChannelType::Twilio,
            );
            // The auth token is present, but the public URL is not.
            c.credentials
                .insert("auth_token".to_string(), TOKEN.to_string());
            c
        });
    })
    .await;

    let params = [("From", "+15550000001"), ("Body", "hi")];
    let signature = twilio_signature(TOKEN, &twilio_webhook_url(port), &params);

    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/twilio", base(port)))
        .header("x-twilio-signature", &signature)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&params))
        .send()
        .await
        .expect("POST /webhooks/twilio");
    assert_eq!(
        resp.status(),
        401,
        "without the configured URL nothing can be verified, so nothing may be accepted"
    );
}

/// A throwaway RSA keypair and the JWKS document publishing it.
///
/// Generated per test rather than embedded — a private key committed to the
/// repository would trip the commit gate's secret scan, and a real one would be
/// worse.
#[cfg(any(feature = "googlechat", feature = "teams"))]
fn bearer_test_key() -> (String, serde_json::Value) {
    use base64::Engine as _;
    use rsa::pkcs8::EncodePrivateKey as _;
    use rsa::traits::PublicKeyParts as _;

    let private =
        rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("generate a test key");
    let public = private.to_public_key();

    let pem = private
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .expect("encode the test key")
        .to_string();

    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let jwks = serde_json::json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": "test-key-1",
            "n": b64(&public.n().to_bytes_be()),
            "e": b64(&public.e().to_bytes_be()),
        }]
    });

    (pem, jwks)
}

/// Mint the bearer token a Chat app receives.
#[cfg(feature = "googlechat")]
fn googlechat_token(private_key_pem: &str, issuer: &str, audience: &str, email: &str) -> String {
    let key =
        jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).expect("encoding key");

    let now = chrono::Utc::now().timestamp();
    let claims = serde_json::json!({
        "iss": issuer,
        "aud": audience,
        "email": email,
        "sub": "a-subject",
        "iat": now,
        "exp": now + 300,
    });

    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-key-1".to_string());

    jsonwebtoken::encode(&header, &claims, &key).expect("mint a token")
}

/// The credentials a Google Chat channel needs, pointed at a mock key set.
#[cfg(feature = "googlechat")]
struct GoogleChatFixture {
    issuer: String,
    audience: String,
    client_email: String,
    service_account: String,
    private_key_pem: String,
}

#[cfg(feature = "googlechat")]
const GOOGLE_ISSUER: &str = "chat@system.gserviceaccount.com";

/// Start a mock JWKS endpoint publishing `key`, and return the fixture wired to
/// it. The server must outlive the test that uses it.
#[cfg(feature = "googlechat")]
async fn googlechat_server(key: &serde_json::Value, port: u16) -> (MockServer, GoogleChatFixture) {
    use wiremock::matchers::{method, path};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(key))
        .mount(&server)
        .await;

    let fixture = GoogleChatFixture {
        issuer: GOOGLE_ISSUER.to_string(),
        audience: format!("{}/webhooks/googlechat", base(port)),
        client_email: "bot@project.iam.gserviceaccount.com".to_string(),
        service_account: GOOGLE_ISSUER.to_string(),
        private_key_pem: String::new(),
    };

    (server, fixture)
}

#[cfg(feature = "googlechat")]
fn googlechat_channel_config(
    fixture: &GoogleChatFixture,
    jwks_url: &str,
    allowed: &str,
) -> syscity::gateway::config::ChannelConfig {
    let mut c =
        syscity::gateway::config::ChannelConfig::new(syscity::channels::ChannelType::GoogleChat);
    c.credentials
        .insert("issuer".to_string(), fixture.issuer.clone());
    c.credentials
        .insert("audience".to_string(), fixture.audience.clone());
    c.credentials
        .insert("jwks_url".to_string(), jwks_url.to_string());
    // Deliberately no service account: the two halves are configured
    // separately, and leaving the outbound side unconfigured is what lets the
    // test's recording adapter own the "googlechat" name — so the reply is
    // observable here instead of being posted to the real Chat API.
    c.credentials
        .insert("allowed_service_accounts".to_string(), allowed.to_string());
    c
}

/// A Chat message event from a human, in a named space.
#[cfg(feature = "googlechat")]
fn chat_message_event(user: &str, space: &str, text: &str, sender_type: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type": "MESSAGE",
        "message": {
            "name": format!("{}/messages/M_HTTP_E2E", space),
            "sender": { "name": user, "type": sender_type },
            "text": text,
        },
        "space": { "name": space, "spaceType": "DIRECT_MESSAGE" },
    }))
    .expect("serialize")
}

/// Case 11 — a bearer-token webhook drives the same round trip, and the reply
/// is addressed to the *space* rather than the sender. That distinction is the
/// whole reason the shared inbound tail has a variant taking both: the Chat API
/// cannot send to `users/…`.
#[cfg(feature = "googlechat")]
#[tokio::test]
#[serial]
async fn googlechat_webhook_round_trip_replies_to_the_space() {
    let port = free_port();
    let (private_pem, jwks) = bearer_test_key();
    let (jwks_server, mut fixture) = googlechat_server(&jwks, port).await;
    let jwks_url = format!("{}/jwks", jwks_server.uri());
    fixture.private_key_pem = private_pem.clone();

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.channels.insert(
        "googlechat".to_string(),
        googlechat_channel_config(&fixture, &jwks_url, GOOGLE_ISSUER),
    );

    let gateway = new_test_gateway(config).await;
    let state = gateway.state();
    let (channel, sent) = HttpTestChannel::new("googlechat");
    state
        .channels
        .reply_dispatcher
        .register_channel("googlechat", channel)
        .await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    start_gateway_and_wait_opts(port, gateway, true).await;

    let space = "spaces/S_HTTP_E2E";
    let token = googlechat_token(&private_pem, &fixture.issuer, &fixture.audience, GOOGLE_ISSUER);
    let body = chat_message_event(
        "users/U_HTTP_E2E",
        space,
        "Say exactly 'pong-from-llm' and nothing else.",
        "HUMAN",
    );

    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/googlechat", base(port)))
        .header("authorization", format!("Bearer {}", token))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST /webhooks/googlechat");
    assert_eq!(resp.status(), 200, "a verified webhook must be accepted");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("timed out waiting for the webhook reply on channel 'googlechat'");
        }
        let snapshot = sent.read().await.clone();
        if let Some(reply) = snapshot
            .iter()
            .find(|m| m.content.contains("pong-from-llm"))
        {
            assert_eq!(
                reply.conversation_id.0, space,
                "the reply is addressed to the space, not to the sender or the session"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 12 — a delivery that does not prove it came from an allowed identity is
/// refused. Four ways in, all refused: no token, a token signed by another key,
/// a token for another audience, and a token Google signed for an identity this
/// app does not accept.
#[cfg(feature = "googlechat")]
#[tokio::test]
#[serial]
async fn googlechat_webhook_rejects_anything_that_does_not_prove_identity() {
    let port = free_port();
    let (private_pem, jwks) = bearer_test_key();
    let (jwks_server, mut fixture) = googlechat_server(&jwks, port).await;
    let jwks_url = format!("{}/jwks", jwks_server.uri());
    fixture.private_key_pem = private_pem.clone();

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.channels.insert(
        "googlechat".to_string(),
        googlechat_channel_config(&fixture, &jwks_url, GOOGLE_ISSUER),
    );

    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;
    start_gateway_and_wait_opts(port, gateway, true).await;

    let body = chat_message_event("users/U", "spaces/S", "hi", "HUMAN");

    // A token signed by a key the issuer does not publish.
    let (other_pem, _) = bearer_test_key();
    let wrong_key = googlechat_token(&other_pem, &fixture.issuer, &fixture.audience, GOOGLE_ISSUER);

    // A token minted for a different audience.
    let wrong_audience = googlechat_token(
        &private_pem,
        &fixture.issuer,
        "https://somewhere-else.example/webhooks/googlechat",
        GOOGLE_ISSUER,
    );

    // A token Google legitimately signed — for an identity this app does not
    // accept. The signature is fine; the identity is not.
    let wrong_identity = googlechat_token(
        &private_pem,
        &fixture.issuer,
        &fixture.audience,
        "someone-else@example.com",
    );

    let valid = googlechat_token(&private_pem, &fixture.issuer, &fixture.audience, GOOGLE_ISSUER);

    for (label, auth) in [
        ("no token", None),
        ("not a jwt", Some("Bearer not-a-jwt".to_string())),
        ("signed by another key", Some(format!("Bearer {}", wrong_key))),
        ("wrong audience", Some(format!("Bearer {}", wrong_audience))),
        ("identity not allowed", Some(format!("Bearer {}", wrong_identity))),
    ] {
        let mut req = reqwest::Client::new()
            .post(format!("{}/webhooks/googlechat", base(port)))
            .header("content-type", "application/json");
        if let Some(auth) = &auth {
            req = req.header("authorization", auth);
        }

        let resp = req
            .body(body.clone())
            .send()
            .await
            .expect("POST /webhooks/googlechat");
        assert_eq!(
            resp.status(),
            401,
            "{label}: a delivery that does not prove an allowed identity must be refused"
        );
    }

    // The control: the same request with a good token is accepted, so the four
    // refusals above are the identity check and not a broken route.
    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/googlechat", base(port)))
        .header("authorization", format!("Bearer {}", valid))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST /webhooks/googlechat");
    assert_eq!(resp.status(), 200, "a valid token is accepted");
}

// ─────────────────────────────────────────────
// Microsoft Teams
// ─────────────────────────────────────────────

/// The public cloud's Bot Framework issuer and the app id the test's bot
/// answers as. The service URL is the tenant routing path replies go back to,
/// which is *not* a constant — the regions differ — and so travels with each
/// activity rather than being pinned here.
#[cfg(feature = "teams")]
const TEAMS_ISSUER: &str = "https://api.botframework.com";
#[cfg(feature = "teams")]
const TEAMS_APP_ID: &str = "11111111-2222-3333-4444-555555555555";
#[cfg(feature = "teams")]
const TEAMS_SERVICE_URL: &str = "https://smba.trafficmanager.net/amer/";

/// Mint the Bot Framework token a Teams delivery carries.
///
/// Microsoft signs one token per bot and stamps the connector it was minted for
/// into `serviceUrl`; that claim is what the gateway holds the activity's own
/// `serviceUrl` to, so it is the interesting thing to be able to vary.
#[cfg(feature = "teams")]
fn teams_token(private_key_pem: &str, issuer: &str, app_id: &str, service_url: &str) -> String {
    let key =
        jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).expect("encoding key");

    let now = chrono::Utc::now().timestamp();
    let claims = serde_json::json!({
        "iss": issuer,
        "aud": app_id,
        // All one word, all lower case: this is the claim the Bot Framework
        // SDKs look up, and `serviceUrl` is only the activity field's spelling.
        "serviceurl": service_url,
        "sub": "a-subject",
        "iat": now,
        "exp": now + 300,
    });

    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-key-1".to_string());

    jsonwebtoken::encode(&header, &claims, &key).expect("mint a token")
}

/// Start a mock key set publishing `key`. The server must outlive the test that
/// uses it.
#[cfg(feature = "teams")]
async fn teams_jwks_server(key: &serde_json::Value) -> MockServer {
    use wiremock::matchers::{method, path};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(key))
        .mount(&server)
        .await;

    server
}

/// The credentials a Teams channel needs to verify deliveries, pointed at a mock
/// key set.
#[cfg(feature = "teams")]
fn teams_channel_config(jwks_url: &str) -> syscity::gateway::config::ChannelConfig {
    let mut c = syscity::gateway::config::ChannelConfig::new(syscity::channels::ChannelType::Teams);
    c.credentials
        .insert("app_id".to_string(), TEAMS_APP_ID.to_string());
    c.credentials
        .insert("issuer".to_string(), TEAMS_ISSUER.to_string());
    c.credentials
        .insert("jwks_url".to_string(), jwks_url.to_string());
    // Deliberately no app registration: the two halves are configured
    // separately, and leaving the outbound side unconfigured is what lets the
    // test's recording adapter own the "teams" name — so the reply is observable
    // here instead of being posted to the real Bot Framework connector.
    c
}

/// A message activity from a human, in a named conversation.
#[cfg(feature = "teams")]
fn teams_activity(from: &str, conversation: &str, text: &str, service_url: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type": "message",
        "id": "ACT_HTTP_E2E",
        "text": text,
        "serviceUrl": service_url,
        "channelId": "msteams",
        "from": { "id": from, "aadObjectId": "aad-object-id" },
        "recipient": { "id": format!("28:{}", TEAMS_APP_ID) },
        "conversation": { "id": conversation, "conversationType": "channel" },
    }))
    .expect("serialize")
}

/// Case 13 — a Bot Framework activity drives the same round trip, and the reply
/// is addressed to the *conversation* rather than the sender. The address is a
/// pair — the tenant's connector host and the conversation — because that is
/// what the connector takes, and the service URL is per-tenant rather than a
/// constant.
#[cfg(feature = "teams")]
#[tokio::test]
#[serial]
async fn teams_webhook_round_trip_replies_to_the_conversation() {
    let port = free_port();
    let (private_pem, jwks) = bearer_test_key();
    let jwks_server = teams_jwks_server(&jwks).await;
    let jwks_url = format!("{}/jwks", jwks_server.uri());

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config
        .channels
        .insert("teams".to_string(), teams_channel_config(&jwks_url));

    let gateway = new_test_gateway(config).await;
    let state = gateway.state();
    let (channel, sent) = HttpTestChannel::new("teams");
    state
        .channels
        .reply_dispatcher
        .register_channel("teams", channel)
        .await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    start_gateway_and_wait_opts(port, gateway, true).await;

    let conversation = "19:conv@thread.tacv2";
    let token = teams_token(&private_pem, TEAMS_ISSUER, TEAMS_APP_ID, TEAMS_SERVICE_URL);
    let body = teams_activity(
        "29:user",
        conversation,
        "Say exactly 'pong-from-llm' and nothing else.",
        TEAMS_SERVICE_URL,
    );

    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/teams", base(port)))
        .header("authorization", format!("Bearer {}", token))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST /webhooks/teams");
    assert_eq!(resp.status(), 200, "a verified webhook must be accepted");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() >= deadline {
            dump_captured_logs();
            panic!("timed out waiting for the webhook reply on channel 'teams'");
        }
        let snapshot = sent.read().await.clone();
        if let Some(reply) = snapshot
            .iter()
            .find(|m| m.content.contains("pong-from-llm"))
        {
            assert_eq!(
                reply.conversation_id.0,
                format!("{}|{}", TEAMS_SERVICE_URL, conversation),
                "the reply is addressed to the tenant's connector and the conversation, \
                 not to the sender or the session"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 14 — a delivery that does not prove it came from *this* bot's connector
/// is refused.
///
/// The last two are the ones Teams adds over Google Chat: a token Microsoft
/// legitimately signed for another app is refused by the audience check, and one
/// it signed for another tenant's connector — same issuer, same app, same keys —
/// is refused only because the `serviceUrl` claim and the activity disagree.
#[cfg(feature = "teams")]
#[tokio::test]
#[serial]
async fn teams_webhook_rejects_anything_that_does_not_prove_the_service() {
    let port = free_port();
    let (private_pem, jwks) = bearer_test_key();
    let jwks_server = teams_jwks_server(&jwks).await;
    let jwks_url = format!("{}/jwks", jwks_server.uri());

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config
        .channels
        .insert("teams".to_string(), teams_channel_config(&jwks_url));

    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;
    start_gateway_and_wait_opts(port, gateway, true).await;

    let body = teams_activity("29:user", "19:conv@thread.tacv2", "hi", TEAMS_SERVICE_URL);

    // A token signed by a key the issuer does not publish.
    let (other_pem, _) = bearer_test_key();
    let wrong_key = teams_token(&other_pem, TEAMS_ISSUER, TEAMS_APP_ID, TEAMS_SERVICE_URL);

    // A token minted for a different bot.
    let wrong_app = teams_token(&private_pem, TEAMS_ISSUER, "another-app-id", TEAMS_SERVICE_URL);

    // A token from a different issuer.
    let wrong_issuer =
        teams_token(&private_pem, "https://elsewhere.example", TEAMS_APP_ID, TEAMS_SERVICE_URL);

    // Signed by the right key, for the right app, by the right issuer — and for
    // another tenant's connector.
    let other_tenant = "https://smba.trafficmanager.net/emea/";
    let wrong_service = teams_token(&private_pem, TEAMS_ISSUER, TEAMS_APP_ID, other_tenant);

    let valid = teams_token(&private_pem, TEAMS_ISSUER, TEAMS_APP_ID, TEAMS_SERVICE_URL);

    for (label, auth) in [
        ("no token", None),
        ("not a jwt", Some("Bearer not-a-jwt".to_string())),
        ("signed by another key", Some(format!("Bearer {}", wrong_key))),
        ("another app", Some(format!("Bearer {}", wrong_app))),
        ("another issuer", Some(format!("Bearer {}", wrong_issuer))),
        ("another tenant's connector", Some(format!("Bearer {}", wrong_service))),
    ] {
        let mut req = reqwest::Client::new()
            .post(format!("{}/webhooks/teams", base(port)))
            .header("content-type", "application/json");
        if let Some(auth) = &auth {
            req = req.header("authorization", auth);
        }

        let resp = req
            .body(body.clone())
            .send()
            .await
            .expect("POST /webhooks/teams");
        assert_eq!(
            resp.status(),
            401,
            "{label}: a delivery that does not prove it came from this bot's connector must be \
             refused"
        );
    }

    // The claim check on its own: a token that verifies against every other
    // check, carrying an activity whose serviceUrl says something else.
    let elsewhere = teams_activity("29:user", "19:conv@thread.tacv2", "hi", other_tenant);
    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/teams", base(port)))
        .header("authorization", format!("Bearer {}", valid))
        .header("content-type", "application/json")
        .body(elsewhere)
        .send()
        .await
        .expect("POST /webhooks/teams");
    assert_eq!(resp.status(), 401, "the token's serviceUrl claim and the activity's must agree");

    // The control: the same request with a good token is accepted, so the
    // refusals above are the checks and not a broken route.
    let resp = reqwest::Client::new()
        .post(format!("{}/webhooks/teams", base(port)))
        .header("authorization", format!("Bearer {}", valid))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("POST /webhooks/teams");
    assert_eq!(resp.status(), 200, "a valid token is accepted");
}
