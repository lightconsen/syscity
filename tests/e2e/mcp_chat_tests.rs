//! MCP chat journey tests.
//!
//! The MCP side has manager-level e2e (`tests/mcp_e2e.rs`: connect, list,
//! reconnect against a real stdio server) and the registry has a noop
//! registration test — but nothing covered the seam between them: a chat
//! prompt triggering an **MCP** tool through the agent registry, with the
//! tool's declared annotations driving its approval posture.
//!
//! Journey here: a mock MCP server (python stdio fixture, extended with a
//! `tools/call` handler and a `readOnlyHint` annotation) is configured in
//! `[mcp.servers]`, the gateway boots and registers `mcp__mock__echo`, and a
//! chat prompt drives the mock provider to call it. No auto-approver runs:
//! the annotated tool is read-only and must execute without any approval —
//! if the annotation were dropped, the tool would gate (no approver present)
//! and the turn would time out, failing the test.

use super::*;

use std::path::PathBuf;

use syscity::mcp::{McpServerConfig, McpTransport};

/// Skip early when python3 is unavailable (same stance as `tests/mcp_e2e.rs`).
async fn require_python3() -> bool {
    tokio::process::Command::new("python3")
        .arg("--version")
        .output()
        .await
        .is_ok()
}

/// MockProvider that requests the MCP tool first, then answers finally.
fn mcp_mock(tool: &str, arguments: serde_json::Value) -> MockProvider {
    let tool = tool.to_string();
    let arguments = arguments.to_string();
    MockProvider::new().with_callback(move |messages| {
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        if messages.iter().any(|m| m.role == Role::Tool) {
            return ProviderMessage::assistant("Done.");
        }
        ProviderMessage::assistant("Let me echo that.").with_tool_calls(vec![ToolCall {
            id: "call_mcp_journey_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: tool.clone(),
                arguments: arguments.clone(),
            },
            index: None,
            result: None,
        }])
    })
}

#[tokio::test]
#[serial]
async fn chat_invokes_a_registered_mcp_tool_and_replies() {
    if !require_python3().await {
        eprintln!("Skipping MCP chat journey: python3 not available");
        return;
    }

    let port = free_port();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture = manifest.join("tests/fixtures/mock_mcp_server.py");

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.mcp.servers.insert(
        "mock".to_string(),
        McpServerConfig {
            transport: McpTransport::Stdio,
            command: Some("python3".to_string()),
            args: vec![fixture.to_string_lossy().to_string()],
            timeout_secs: 10,
            ..Default::default()
        },
    );

    let gateway = syscity::gateway::Gateway::new(config, None)
        .await
        .expect("Failed to create test gateway");
    // Grab the state handle before start moves the gateway into its task.
    let state = gateway.state();
    let router = gateway.model_router();
    let mock = mcp_mock("mcp__mock__echo", json!({ "message": "mcp-journey-marker" }));
    register_mock_provider_with_model(&router, mock, "mock-model").await;

    start_gateway_and_wait_opts(port, gateway, false).await;

    // Boot connects to the MCP server in the background; wait for the
    // tool to land in the agent registry before chatting.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "MCP tool mcp__mock__echo was never registered"
        );
        if state.tools.registry.has("mcp__mock__echo") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(
            &sid,
            "Use the mcp__mock__echo tool with message \
                           'mcp-journey-marker', then report what it echoed.",
        )
        .await;

    let payload = client
        .wait_for_event("chat.final", 30)
        .await
        .expect("chat must complete through the MCP tool call");
    let response = payload
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(response.contains("Done."), "expected the mock's final answer, got: {response}");
}
