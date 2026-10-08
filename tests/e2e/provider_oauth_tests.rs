//! Provider OAuth journey: WS starts the flow, the gateway's own HTTP callback
//! completes it, the credential lands in the secret store, and the client hears
//! about it.
//!
//! What this pins that the unit tests cannot: the two entrances agree. The flow
//! is remembered by `providers.auth_start` (a WS method) and completed by
//! `/oauth/provider/callback` (an HTTP route), so only a test that boots both
//! surfaces proves they are looking at one pending map and one secret store.
//! It is also the only place the redirect target is exercised as a real URL the
//! gateway actually serves — unit tests assert the string, not that a request
//! to it works.

use super::*;
use syscity::dirs::SyscityPaths;
use syscity::gateway::GatewayOptions;
use syscity::model_router::OAuthConfig;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Add a provider whose credential comes from OAuth, pointed at `token_url`.
fn add_oauth_provider(config: &mut GatewayConfig, id: &str, token_url: String) {
    config.providers.insert(
        id.to_string(),
        ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec!["oauth-model".to_string()],
            default_model: "oauth-model".to_string(),
            api_key: "unused-for-oauth".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: Some(OAuthConfig {
                client_id: "e2e-client".to_string(),
                auth_url: "https://provider.example/authorize".to_string(),
                token_url,
                scope: None,
                client_secret: None,
                redirect_base: None,
                refresh_token: None,
            }),
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        },
    );
}

/// The `state` the gateway issued, read back out of the authorization URL —
/// the same value the provider would echo to the callback.
fn state_of(auth_url: &str) -> String {
    auth_url
        .split("state=")
        .nth(1)
        .and_then(|tail| tail.split('&').next())
        .expect("the authorization URL carries a state")
        .to_string()
}

/// Boot a gateway with an OAuth provider whose token endpoint is `token_url`,
/// on a **private** layout root.
///
/// The root matters here and is not incidental. The e2e harness leaves
/// `GatewayOptions::paths` unset, so a gateway derives its layout from
/// `SYSCITY_HOME` / `~/.syscity` — which means a credential this test stores
/// lands in the developer's real config directory and **survives the run**, so
/// the next run starts with a provider already authorized. Passing a temp root
/// (`GatewayOptions::paths`, documented as the way to keep a gateway off the
/// real home) makes the store, the agents, and everything else local to the
/// test. The returned `TempDir` must be held for the test's duration.
async fn start_oauth_gateway(port: u16, id: &str, token_url: String) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temp layout root");
    let paths = Arc::new(SyscityPaths::from_root(root.path()));

    let mut config = test_config(port, false);
    add_oauth_provider(&mut config, id, token_url);
    let gateway = Gateway::with_options(
        config,
        None,
        GatewayOptions {
            paths: Some(paths),
            ..Default::default()
        },
    )
    .await
    .expect("Failed to create test gateway");
    start_gateway_and_wait(port, gateway).await;

    root
}

async fn token_endpoint(server: &MockServer) {
    // Exactly one exchange, verified when the mock server drops: a flow that
    // exchanged twice (or a replay that got as far as the network) fails here.
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "e2e-access",
            "refresh_token": "e2e-refresh",
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
#[serial]
async fn provider_oauth_journey_authorizes_and_stores_the_credential() {
    const ID: &str = "grok-journey";

    let token_server = MockServer::start().await;
    token_endpoint(&token_server).await;

    let port = free_port();
    let _root = start_oauth_gateway(port, ID, format!("{}/token", token_server.uri())).await;
    let mut client = FrontendSimulator::connect(port).await;

    // Before: not authorized, nothing pending.
    let before = client
        .request("providers.auth_status", json!({ "id": ID }))
        .await;
    let payload = resp_payload(&before).expect("payload");
    assert_eq!(payload["authorized"], json!(false));
    assert_eq!(payload["pending"], json!(null));

    // 1. WS starts the flow and hands back a URL.
    let started = client
        .request("providers.auth_start", json!({ "id": ID }))
        .await;
    assert_eq!(started["ok"], json!(true), "{started}");
    let payload = resp_payload(&started).expect("payload");
    let auth_url = payload["auth_url"].as_str().expect("auth_url").to_string();
    let state = state_of(&auth_url);
    assert!(!state.is_empty());

    // 2. The browser lands on the gateway's own callback route.
    let callback = reqwest::get(format!(
        "http://127.0.0.1:{port}/oauth/provider/callback?code=e2e-code&state={state}"
    ))
    .await
    .expect("callback request");
    assert_eq!(callback.status(), 200);

    // 3. The client is told, without polling for it.
    let event = client
        .wait_for_event("providers.auth_complete", 10)
        .await
        .expect("providers.auth_complete event");
    assert_eq!(event["provider"], json!(ID));

    // 4. And the provider now reads as authorized — that answer comes from the
    //    secret store, so it also proves the token was persisted.
    let after = client
        .request("providers.auth_status", json!({ "id": ID }))
        .await;
    let payload = resp_payload(&after).expect("payload");
    assert_eq!(payload["authorized"], json!(true));
    assert_eq!(payload["pending"], json!(null));
}

#[tokio::test]
#[serial]
async fn provider_oauth_callback_refuses_a_replayed_state() {
    const ID: &str = "grok-replay";

    let token_server = MockServer::start().await;
    token_endpoint(&token_server).await;

    let port = free_port();
    let _root = start_oauth_gateway(port, ID, format!("{}/token", token_server.uri())).await;
    let mut client = FrontendSimulator::connect(port).await;

    let started = client
        .request("providers.auth_start", json!({ "id": ID }))
        .await;
    let auth_url = resp_payload(&started).expect("payload")["auth_url"]
        .as_str()
        .expect("auth_url")
        .to_string();
    let state = state_of(&auth_url);

    let url =
        format!("http://127.0.0.1:{port}/oauth/provider/callback?code=e2e-code&state={state}");

    let first = reqwest::get(&url).await.expect("first callback");
    assert_eq!(first.status(), 200);

    // The state is single-use: a captured callback replayed later must not run
    // the exchange again — the 400 says it was refused, and the mock's
    // single-exchange expectation fails the test if it reached the network.
    let replay = reqwest::get(&url).await.expect("replayed callback");
    assert_eq!(replay.status(), 400);
}
