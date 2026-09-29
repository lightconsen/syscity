//! Admin WS surface smoke tests.
//!
//! 19 of the 24 `ws/admin_ws` handler modules have no tests of their own, and
//! the SPA talks to the gateway almost exclusively through these methods. The
//! journeys elsewhere in this tree exercise a handful incidentally
//! (`chat.*`, `approvals.*`, `mcp.*`); the rest — status, audit, providers,
//! skills, cron, security, mention, system, update, onboarding — would fail
//! only in front of a user.
//!
//! The lists below are partitioned by observed behaviour (each method was
//! called with no arguments against a real gateway):
//!   1. parameterless methods that must answer `ok`;
//!   2. methods with required parameters that must refuse with
//!      `INVALID_PARAMS` — a structured refusal, never a panic or a silent
//!      `ok`;
//!   3. methods whose parameters were accepted and whose *resource* is absent
//!      in a bare test gateway (`NOT_FOUND` / `UNAVAILABLE`) — proof the
//!      handler is wired and validating, rather than an internal error.

use super::*;

/// Start a bare gateway (mock provider, no channels/extras) for the surface
/// sweep.
async fn start_admin_gateway(port: u16) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    let gateway = Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;
    start_gateway_and_wait_opts(port, gateway, true).await;
}

/// Case 1 — every parameterless admin method the SPA calls answers `ok` with
/// a payload.
#[tokio::test]
#[serial]
async fn parameterless_admin_methods_answer_over_ws() {
    let port = free_port();
    start_admin_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    for method in [
        // runtime / control plane
        "status.get",
        "system.presence",
        // operator surfaces
        "audit.recent",
        "audit.all",
        "approvals.list",
        "update.status",
        // models + providers
        "providers.list",
        "providers.usage",
        // skills / plugins
        "skills.list",
        "plugins.list",
        // scheduling
        "cron.list",
        // security / policy
        "security.status",
        "security.allowlist.list",
        "security.gate.list",
        "mention.policy",
        "mention.allowlist",
        "mention.blocklist",
        // the SPA's sidebar sources
        "channels.list",
        "agents.list",
        "agents.registry",
        // onboarding + MCP
        "onboarding.status",
        "mcp.list",
        "mcp.presets",
    ] {
        let frame = client.request(method, json!({})).await;
        assert_eq!(
            frame.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "{method} must answer ok; got {frame}"
        );
        assert!(frame.get("payload").is_some(), "{method} must carry a payload; got {frame}");
    }
}

/// Case 2 — methods with required parameters refuse a bare call with
/// `INVALID_PARAMS`, not a crash and not a silent success.
#[tokio::test]
#[serial]
async fn parameterized_admin_methods_refuse_a_bare_call() {
    let port = free_port();
    start_admin_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    for (method, missing) in [
        ("agents.get", "agent_id"),
        ("agents.default", "agent_id"),
        ("approvals.get", "id"),
        ("cron.get", "id"),
        ("memory.search", "query"),
        ("providers.fallback", "model_id"),
        ("traces.get", "turn_id"),
    ] {
        let frame = client.request(method, json!({})).await;
        assert_eq!(
            frame.get("ok").and_then(|v| v.as_bool()),
            Some(false),
            "{method} must refuse a bare call; got {frame}"
        );
        let code = frame
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        // One refusal shape for one caller mistake. Handlers used to differ —
        // a missing `id` read as `""` and surfaced as `NOT_FOUND` from some,
        // `INVALID_PARAMS` from others — so a client could not tell a bad
        // call from a missing resource.
        assert_eq!(
            code, "INVALID_PARAMS",
            "{method} must refuse a bare call (missing `{missing}`) with INVALID_PARAMS; \
             got {frame}"
        );
    }
}

/// Case 3 — handlers whose parameters were accepted but whose resource is
/// absent answer a structured NOT_FOUND / UNAVAILABLE. This is the wiring
/// proof: a missing route would answer UNKNOWN_METHOD, and a broken handler
/// would answer INTERNAL_ERROR.
#[tokio::test]
#[serial]
async fn resource_lookup_methods_answer_structured_absences() {
    let port = free_port();
    start_admin_gateway(port).await;
    let mut client = FrontendSimulator::connect(port).await;

    for (method, params) in [
        ("providers.health", json!({ "id": "no-such-provider" })),
        ("mcp.tools", json!({ "server_id": "no-such-server" })),
        ("skills.get", json!({ "name": "no-such-skill" })),
        ("cron.logs", json!({ "id": "no-such-job" })),
        ("memory.collections", json!({})),
    ] {
        let frame = client.request(method, params).await;
        let code = frame
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert!(
            matches!(code, "NOT_FOUND" | "UNAVAILABLE"),
            "{method} must answer a structured absence, got {frame}"
        );
    }
}
