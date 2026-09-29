//! Authentication-mode journey tests.
//!
//! The whole e2e tree runs `AuthMode::None` (`tests/e2e/mod.rs::test_config`),
//! so every rejection path lived only in in-file unit tests. A regression in
//! the upgrade gate or the REST middleware would sail through CI. These tests
//! boot the gateway in `AuthMode::Token` and drive both surfaces with and
//! without credentials.
//!
//! Contract pinned here (read from `ws/core.rs::validate_ws_upgrade_request`
//! and `ws/handshake.rs::resolve_token_auth`):
//! - The **upgrade** is gated: no credential, wrong `?token=`, or an invalid
//!   ticket is refused with HTTP 401 before any WebSocket exists.
//! - The **handshake** re-validates under this mode: the `connect` frame must
//!   carry the token in `params.auth.token`.
//!
//! Known gap (not pinned by a test, reported instead): a client that
//! authenticated with `?ticket=` cannot satisfy the frame-level check — the
//! handshake discards the upgrade's `pre_validated_auth` outside
//! `AuthMode::None`, so the ticket flow lands with zero scopes.

use super::*;
use syscity::gateway::protocol::AuthMode;

const TOKEN: &str = "e2e-shared-secret";

/// Start a token-authed gateway and wait for readiness over **HTTP** — the
/// plain-WebSocket readiness probe the shared helper uses cannot work here,
/// because the upgrade itself requires a credential.
async fn start_token_gateway(port: u16) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.security.auth_mode = AuthMode::Token;
    config.security.shared_token = Some(TOKEN.to_string());
    // The default `shared_token_scopes` is deliberately read-mostly
    // (chat + read + pairing). Grant write so the journey can create a
    // session — `test_config` does the same for the anonymous local client.
    config.security.shared_token_scopes = vec![
        "chat".to_string(),
        "read".to_string(),
        "write".to_string(),
        "admin".to_string(),
    ];

    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    let state = gateway.state();
    tokio::spawn(async move {
        let _ = gateway.start().await;
    });

    // Poll the unauthenticated liveness probe until the listener answers.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "gateway never answered /live on port {port}"
        );
        if let Ok(resp) = reqwest::get(format!("http://127.0.0.1:{}/live", port)).await {
            if resp.status().is_success() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(state);
}

/// The status of a refused WS upgrade, or `None` when the upgrade succeeded.
async fn upgrade_status(url: &str) -> Option<u16> {
    match tokio_tungstenite::connect_async(url).await {
        Ok(_) => None,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Some(resp.status().as_u16()),
        Err(e) => panic!("unexpected upgrade error: {e}"),
    }
}

/// Case 1 — the upgrade is the gate: no credential and a wrong `?token=` are
/// both refused 401, without a WebSocket ever existing.
#[tokio::test]
#[serial]
async fn token_auth_refuses_the_upgrade_without_a_valid_credential() {
    let port = free_port();
    start_token_gateway(port).await;

    let bare = upgrade_status(&format!("ws://127.0.0.1:{}/ws", port)).await;
    assert_eq!(bare, Some(401), "a credential-less upgrade must be refused 401");

    let wrong = upgrade_status(&format!("ws://127.0.0.1:{}/ws?token=nope", port)).await;
    assert_eq!(wrong, Some(401), "a wrong ?token= must be refused 401");
}

/// Case 2 — a valid token clears both gates and the connection runs a turn.
#[tokio::test]
#[serial]
async fn token_auth_accepts_a_valid_token_and_completes_a_turn() {
    let port = free_port();
    start_token_gateway(port).await;

    // Upgrade with the token in the query string (the documented fallback for
    // clients that cannot set an Authorization header on an upgrade).
    let url = format!("ws://127.0.0.1:{}/ws?token={}", port, TOKEN);
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("a valid ?token= must clear the upgrade gate");
    let (mut write, mut read) = ws_stream.split();

    // Under this mode the handshake re-validates, so the frame carries the
    // token too.
    write
        .send(Message::Text(
            serde_json::json!({
                "type": "req",
                "id": "connect-1",
                "method": "connect",
                "params": {
                    "protocol_version": 1,
                    "scopes": ["chat", "read", "write", "admin"],
                    "auth": { "token": TOKEN }
                }
            })
            .to_string(),
        ))
        .await
        .expect("send connect");

    let msg = timeout(Duration::from_secs(10), read.next())
        .await
        .expect("handshake answer")
        .expect("stream open")
        .expect("frame");
    let response: serde_json::Value =
        serde_json::from_str(msg.to_text().expect("text")).expect("JSON");
    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "the shared token must authenticate the handshake: {response}"
    );

    // The socket is now a working client: create a session and run a turn.
    let mut client = FrontendSimulator::connect_token(port, TOKEN).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Say exactly 'pong-from-llm' and nothing else.")
        .await;
    let payload = client
        .wait_for_event("chat.final", 30)
        .await
        .expect("a token-authed client must be able to run a turn");
    assert!(payload.get("response").is_some());
}

/// Case 3 — the REST surface enforces the same credential; probes stay open.
#[tokio::test]
#[serial]
async fn token_auth_gates_rest_but_not_probes() {
    let port = free_port();
    start_token_gateway(port).await;
    let client = reqwest::Client::new();

    let anon = client
        .get(format!("http://127.0.0.1:{}/v1/models", port))
        .send()
        .await
        .expect("GET /v1/models");
    assert_eq!(anon.status(), 401, "an anonymous REST caller must be refused");

    let authed = client
        .get(format!("http://127.0.0.1:{}/v1/models", port))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("GET /v1/models with bearer");
    assert_eq!(authed.status(), 200, "a bearer-authed caller must be served");

    let live = reqwest::get(format!("http://127.0.0.1:{}/live", port))
        .await
        .expect("GET /live");
    assert_eq!(live.status(), 200, "/live stays open for probes");
}

/// Case 4 — the ticket exchange must not be an anonymous bypass.
///
/// `auth_required` defaults to false, so a REST surface keyed only on it left
/// `auth_mode = "token"` open: an anonymous caller could mint a ticket here
/// and that ticket cleared the WS upgrade gate. The middleware now treats a
/// configured auth mode as requiring credentials; this pins the closed hole
/// from both ends.
#[tokio::test]
#[serial]
async fn token_auth_refuses_an_anonymous_ticket_exchange() {
    let port = free_port();
    start_token_gateway(port).await;
    let client = reqwest::Client::new();

    let anon = client
        .post(format!("http://127.0.0.1:{}/api/v1/ws-ticket", port))
        .send()
        .await
        .expect("anonymous POST /api/v1/ws-ticket");
    assert_eq!(
        anon.status(),
        401,
        "minting a ticket must require a credential under token auth"
    );

    // An authed mint still works — the endpoint is gated, not broken.
    let authed = client
        .post(format!("http://127.0.0.1:{}/api/v1/ws-ticket", port))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("authed POST /api/v1/ws-ticket");
    assert_eq!(authed.status(), 200, "an authed caller must still mint a ticket");
    let body: serde_json::Value = authed.json().await.expect("json");
    assert!(
        body["ticket"].as_str().is_some_and(|t| !t.is_empty()),
        "the ticket field must be populated"
    );
}
