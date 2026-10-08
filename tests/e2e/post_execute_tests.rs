use syscity::tools::{PostExecuteDecision, ToolHooks};

use super::*;

/// Mock provider that calls the `time` tool once, then answers.
fn time_mock_provider() -> MockProvider {
    MockProvider::new().with_callback(|messages| {
        // Cache-check prompt is a single user message asking about caching.
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        // If a tool result already exists in the conversation, answer finally.
        if messages.iter().any(|m| m.role == Role::Tool) {
            return ProviderMessage::assistant("Done.");
        }
        ProviderMessage::assistant("Let me check the time.").with_tool_calls(vec![ToolCall {
            id: "call_time_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "time".to_string(),
                arguments: r#"{"action":"now"}"#.to_string(),
            },
            index: None,
            result: None,
        }])
    })
}

/// A post-execute hook that confiscates a tool's result must turn it into an
/// error carrying the feedback — and the model's next request must contain
/// that feedback, never the confiscated output.
#[tokio::test]
#[serial]
async fn test_post_execute_block_feedback_reaches_model() {
    let port = free_port();
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    let mock = time_mock_provider();
    register_mock_provider_with_model(&router, mock.clone(), "mock-model").await;

    // Confiscate every `time` result after the tool has executed.
    gateway
        .tool_registry()
        .set_hooks(ToolHooks::new().post_execute(|name, _args, _result, _ctx, _upstream| {
            let blocked = name == "time";
            async move {
                if blocked {
                    PostExecuteDecision::Block(
                        "Time output withheld by test policy; answer without it.".to_string(),
                    )
                } else {
                    PostExecuteDecision::Accept
                }
            }
        }));

    start_gateway_and_wait(port, gateway).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client.send_chat(&sid, "what time is it?").await;

    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("Expected chat.final event");

    // Inspect what the model actually received on its second request.
    let history = mock.history();
    let tool_contents: Vec<&str> = history
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.as_str())
        .collect();

    assert!(
        tool_contents
            .iter()
            .any(|c| c.contains("Error: Time output withheld by test policy")),
        "model should receive the block feedback; tool messages: {:?}",
        tool_contents
    );
}
