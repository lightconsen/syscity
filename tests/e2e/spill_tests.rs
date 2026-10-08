use std::path::PathBuf;

use super::*;

/// Mock provider that asks the shell for a 40KB output, then answers.
fn big_output_mock_provider() -> MockProvider {
    MockProvider::new().with_callback(|messages| {
        // Cache-check prompt is a single user message asking about caching.
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        if messages.iter().any(|m| m.role == Role::Tool) {
            return ProviderMessage::assistant("Done.");
        }
        // `echo` is a shell builtin, so this works even with a cleared env.
        let command = format!("echo {}", "x".repeat(40_000));
        ProviderMessage::assistant("Running a noisy command.").with_tool_calls(vec![ToolCall {
            id: "call_big_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({ "command": command }).to_string(),
            },
            index: None,
            result: None,
        }])
    })
}

/// An oversized tool output must reach the model as a bounded head/tail
/// preview pointing at a spill file inside the agent workspace — never the
/// raw multi-KB blob — while the full output is preserved on disk.
#[tokio::test]
#[serial]
async fn test_oversized_tool_output_spills_to_workspace_file() {
    let port = free_port();
    let ws = std::env::temp_dir().join(format!("syscity_spill_e2e_{}", port));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create temp workspace");

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.default_agent.workspace_dir = Some(ws.clone());

    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    let mock = big_output_mock_provider();
    register_mock_provider_with_model(&router, mock.clone(), "mock-model").await;

    start_gateway_and_wait(port, gateway).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client.send_chat(&sid, "run the noisy command").await;

    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("Expected chat.final event");

    // The model's second request must carry the preview, not the raw blob.
    let history = mock.history();
    let tool_contents: Vec<&str> = history
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(tool_contents.len(), 1, "exactly one tool result expected");
    let preview = tool_contents[0];
    assert!(
        preview.contains(".syscity/spill/"),
        "preview should point at the spill file: {:.200}",
        preview
    );
    assert!(preview.contains("file_read"), "retrieval hint present");
    assert!(preview.len() < 34_000, "preview must be bounded, got {} bytes", preview.len());

    // The full output is preserved inside the workspace spill dir.
    let spill_dir = ws.join(".syscity").join("spill");
    let files: Vec<_> = std::fs::read_dir(&spill_dir)
        .expect("spill dir exists")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1);
    let on_disk = std::fs::read_to_string(files[0].path()).unwrap();
    assert_eq!(on_disk.len(), 40_001, "40KB of x's plus echo's newline");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Extract the workspace-relative spill path from a spill preview's notice,
/// e.g. `.syscity/spill/abcd1234-file_read.log` (trailing sentence period
/// trimmed).
fn spill_path_from_preview(preview: &str) -> String {
    let start = preview
        .find(".syscity/spill/")
        .expect("spill path in preview");
    let rest = &preview[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    rest[..end].trim_end_matches('.').to_string()
}

/// Mock that drives the two-phase file_read loop:
/// 1. reads `big_path` (no limit) → receives the spill preview;
/// 2. re-reads the spilled artifact at its absolute path → receives the whole
///    content back (the spill exemption, not a re-spill);
/// 3. answers.
fn file_read_spill_mock(big_path: PathBuf, ws: PathBuf) -> MockProvider {
    MockProvider::new().with_callback(move |messages| {
        // Cache-check prompt is a single user message asking about caching.
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        let last_tool = messages.iter().rev().find(|m| m.role == Role::Tool);
        match last_tool {
            None => {
                // Phase 1: read the big file with no limit.
                ProviderMessage::assistant("Reading the big file.").with_tool_calls(vec![
                    ToolCall {
                        id: "call_read_big".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "file_read".to_string(),
                            arguments:
                                serde_json::json!({ "path": big_path.display().to_string() })
                                    .to_string(),
                        },
                        index: None,
                        result: None,
                    },
                ])
            }
            Some(tool) if tool.content.contains(".syscity/spill/") => {
                // Phase 2: re-read the spilled artifact by absolute path.
                let rel = spill_path_from_preview(&tool.content);
                let abs = ws.join(rel);
                ProviderMessage::assistant("Inspecting the spilled file.").with_tool_calls(vec![
                    ToolCall {
                        id: "call_read_spill".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "file_read".to_string(),
                            arguments: serde_json::json!({ "path": abs.display().to_string() })
                                .to_string(),
                        },
                        index: None,
                        result: None,
                    },
                ])
            }
            Some(_) => ProviderMessage::assistant("Done."),
        }
    })
}

/// A `file_read` of a large (non-spill) file must now spill — the exemption is
/// path-based, not name-based — and re-reading the spilled artifact must return
/// the whole content without re-spilling (no read → spill → read loop).
#[tokio::test]
#[serial]
async fn test_large_file_read_spills_and_reread_breaks_loop() {
    let port = free_port();
    let ws = std::env::temp_dir().join(format!("syscity_spill_e2e_{}", port));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create temp workspace");
    let big_path = ws.join("big.txt");
    // Prose, not a repeated char: a long alphanumeric run would be redacted
    // by the content filter as a false-positive "AWS secret access key"
    // (its `[0-9a-zA-Z/+]{40}` pattern matches any 40+ alphanumeric chars).
    let line = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor \
                incididunt ut labore\n";
    let mut big_content = String::new();
    while big_content.len() < 200_000 {
        big_content.push_str(line);
    }
    std::fs::write(&big_path, &big_content).expect("write big file");

    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.default_agent.workspace_dir = Some(ws.clone());

    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    let mock = file_read_spill_mock(big_path.clone(), ws.clone());
    register_mock_provider_with_model(&router, mock.clone(), "mock-model").await;

    start_gateway_and_wait(port, gateway).await;
    let mut client = FrontendSimulator::connect(port).await;

    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client.send_chat(&sid, "read the big file").await;

    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("Expected chat.final event");

    let history = mock.history();
    let tool_contents: Vec<&str> = history
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.as_str())
        .collect();
    assert!(!tool_contents.is_empty(), "expected tool results in history");

    // Phase 1 result: bounded preview pointing at the spill file.
    let first = tool_contents[0];
    assert!(first.contains(".syscity/spill/"), "large file_read must spill: {:.200}", first);
    assert!(first.len() < 34_000, "preview must be bounded, got {} bytes", first.len());

    // Phase 2 result: whole content returned, no second spill.
    let last = tool_contents[tool_contents.len() - 1];
    assert!(
        last.len() > 100_000,
        "re-read must return the whole content, got {}",
        last.len()
    );
    assert!(
        !last.contains(".syscity/spill/"),
        "re-reading a spilled artifact must not re-spill"
    );

    // Only one spill file on disk, holding the full original content.
    let spill_dir = ws.join(".syscity").join("spill");
    let files: Vec<_> = std::fs::read_dir(&spill_dir)
        .expect("spill dir exists")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1, "exactly one spill file expected");
    let on_disk = std::fs::read_to_string(files[0].path()).unwrap();
    assert_eq!(on_disk, big_content, "full original content preserved");

    let _ = std::fs::remove_dir_all(&ws);
}
