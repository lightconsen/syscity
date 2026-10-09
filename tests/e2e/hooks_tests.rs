//! End-to-end tests for the CC-compatible shell hooks bridge (`~/.syscity/hooks.json`).
//!
//! These drive a real gateway loaded with a per-port `hooks.json` via
//! `Gateway::with_options`. Each case asserts on what the model actually
//! receives (its history) and on sentinel files that only the tool *body*
//! writes — proving a deny/block intercepts before/after execution.

use std::path::PathBuf;

use syscity::gateway::{Gateway, GatewayOptions};
use syscity::providers::{FunctionCall, Role, ToolCall};

use super::*;

/// Write a `hooks.json` document (with `version: 1`) to a per-port path and
/// return it. The file is read once at gateway startup.
fn write_hooks_file(port: u16, hooks: serde_json::Value) -> PathBuf {
    let doc = json!({ "version": 1, "hooks": hooks });
    let path = std::env::temp_dir().join(format!("syscity_hooks_e2e_{}.json", port));
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).expect("write hooks.json");
    path
}

/// Absolute path for a per-port sentinel file under the OS temp dir.
fn sentinel_path(label: &str, port: u16) -> PathBuf {
    std::env::temp_dir().join(format!("syscity_hooks_{}_{}.marker", label, port))
}

/// A MockProvider that drives a two-turn conversation: first turn requests the
/// `shell` tool with `command`, second turn answers after seeing the tool
/// result. Handles the NOCACHE cache-check prompt automatically.
fn shell_cmd_mock(command: &str) -> MockProvider {
    let command = command.to_string();
    MockProvider::new().with_callback(move |messages| {
        // Cache-check prompt is a single user message asking about caching.
        if messages.len() == 1 && messages[0].content.contains("NOCACHE") {
            return ProviderMessage::assistant("NOCACHE");
        }
        // If a tool result already exists in the conversation, answer finally.
        if messages.iter().any(|m| m.role == Role::Tool) {
            return ProviderMessage::assistant("Done.");
        }
        ProviderMessage::assistant("I'll run that for you.").with_tool_calls(vec![ToolCall {
            id: "call_hook_shell_1".to_string(),
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

/// Build a gateway loaded with `hooks_file`, register the mock provider as
/// `mock-model`, and start it.
async fn start_hooked_gateway(port: u16, hooks: PathBuf, mock: MockProvider) {
    start_hooked_gateway_opts(port, hooks, mock, true).await;
}

/// Same, with control over the auto-approver. The ask case needs it off:
/// the test resolves the raised approval itself.
async fn start_hooked_gateway_opts(
    port: u16,
    hooks: PathBuf,
    mock: MockProvider,
    auto_approve: bool,
) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();

    let gateway = Gateway::with_options(
        config,
        None,
        GatewayOptions {
            hooks_file: Some(hooks),
            // Same hermetic root as every other e2e gateway. Without it the
            // gateway resolves the real `~/.syscity` and creates *its* workspace
            // there, while the shell tool's working directory is the process
            // default (`test_paths_root()`), which nobody created — so the
            // fenced shell cannot spawn and a test that expects the tool body to
            // run sees no sentinel.
            paths: Some(test_paths_root()),
            ..Default::default()
        },
    )
    .await
    .expect("Failed to create hooked test gateway");

    let router = gateway.model_router();
    register_mock_provider_with_model(&router, mock, "mock-model").await;

    start_gateway_and_wait_opts(port, gateway, auto_approve).await;
}
/// must surface the denial to the model and keep the tool body from running.
#[tokio::test]
#[serial]
async fn pre_tool_use_deny_blocks_shell_and_skips_body() {
    let port = free_port();
    let sentinel = sentinel_path("deny", port);
    let _ = std::fs::remove_file(&sentinel);

    let hooks = write_hooks_file(
        port,
        json!({
            "PreToolUse": [
                { "matcher": "*", "hooks": [ {
                    "type": "command",
                    "command": "printf '{\"permission\":\"deny\",\"reason\":\"blocked-by-hook\"}'"
                } ] }
            ]
        }),
    );
    let mock = shell_cmd_mock(&format!("echo denied-ran > '{}'", sentinel.display()));
    start_hooked_gateway(port, hooks, mock.clone()).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the shell tool to write a sentinel file.")
        .await;
    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("Expected chat.final event");

    // The deny reason must reach the model as a tool-message error…
    let history = mock.history();
    let tool_contents: Vec<String> = history
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .collect();
    assert!(
        tool_contents.iter().any(|c| c.contains("blocked-by-hook")),
        "denial reason must reach the model; tool messages: {:?}",
        tool_contents
    );

    // …and the tool body must never have executed.
    assert!(!sentinel.exists(), "shell body must not run when the PreToolUse hook denies it");
}

/// Case 2 — PostToolUse block 没收结果: the hook confiscates a tool's output,
/// so the model sees the block feedback and never the original output.
#[tokio::test]
#[serial]
async fn post_tool_use_block_withholds_result_from_model() {
    let port = free_port();
    let marker = "SECRET-HOOK-MARKER-41221";

    let hooks = write_hooks_file(
        port,
        json!({
            "PostToolUse": [
                { "matcher": "shell", "hooks": [ {
                    "type": "command",
                    "command": "printf '{\"decision\":\"block\",\"reason\":\"withheld\"}'"
                } ] }
            ]
        }),
    );
    let mock = shell_cmd_mock(&format!("echo {}", marker));
    start_hooked_gateway(port, hooks, mock.clone()).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the shell tool to print a secret marker.")
        .await;
    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("Expected chat.final event");

    let history = mock.history();
    let tool_contents: Vec<String> = history
        .iter()
        .flat_map(|req| req.messages.iter())
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone())
        .collect();
    assert!(
        tool_contents.iter().any(|c| c.contains("withheld")),
        "block feedback must reach the model; tool messages: {:?}",
        tool_contents
    );
    assert!(
        !tool_contents.iter().any(|c| c.contains(marker)),
        "confiscated tool output must NOT reach the model; tool messages: {:?}",
        tool_contents
    );
}

/// Case 3 — UserPromptSubmit block 拒消息: a blocked message must surface as
/// `chat.error`, never reach the agent, and produce no `chat.final`.
#[tokio::test]
#[serial]
async fn user_prompt_submit_block_rejects_message() {
    let port = free_port();

    let hooks = write_hooks_file(
        port,
        json!({
            "UserPromptSubmit": [
                { "hooks": [ {
                    "type": "command",
                    "command": "printf '{\"decision\":\"block\",\"reason\":\"prompt-blocked\"}'"
                } ] }
            ]
        }),
    );
    let mock = llm_mock_provider_for_streaming();
    start_hooked_gateway(port, hooks, mock.clone()).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "This message is blocked by the prompt gate.")
        .await;

    let err = client
        .wait_for_event("chat.error", 10)
        .await
        .expect("Expected chat.error event");
    assert_eq!(err.get("message").and_then(|v| v.as_str()), Some("prompt-blocked"));

    let final_ev = client.wait_for_event("chat.final", 2).await;
    assert!(final_ev.is_none(), "no chat.final for a blocked message");

    // A background session-title generation may call the provider for the
    // first message even when it is blocked (ws/chat.rs auto-naming), so
    // history cannot be asserted empty. The agent's own first LLM call is the
    // cache-check prompt ("NOCACHE") — its absence proves the agent never
    // started processing the blocked message.
    let hist = mock.history();
    assert!(
        !hist
            .iter()
            .flat_map(|req| req.messages.iter())
            .any(|m| m.content.contains("NOCACHE")),
        "agent must never run for a blocked message"
    );
}

/// Case 4 — Stop 执行: the fire-and-forget Stop hook runs after the turn ends,
/// appending its sentinel to a log file.
#[tokio::test]
#[serial]
async fn stop_hook_fires_after_turn_ends() {
    let port = free_port();
    let log = sentinel_path("stop", port);
    let _ = std::fs::remove_file(&log);

    let hooks = write_hooks_file(
        port,
        json!({
            "Stop": [
                { "hooks": [ {
                    "type": "command",
                    "command": format!("printf 'stopped' >> '{}'", log.display())
                } ] }
            ]
        }),
    );
    let mock = llm_mock_provider_for_streaming();
    start_hooked_gateway(port, hooks, mock).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client.send_chat(&sid, "Say something short.").await;
    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("Expected chat.final event");

    // The Stop hook is fire-and-forget: poll briefly for the sentinel.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let done = std::fs::read_to_string(&log)
            .map(|s| s.contains("stopped"))
            .unwrap_or(false);
        if done {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "Stop hook sentinel never appeared");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Case 5 — fail-open: a crashing PreToolUse hook (non-zero exit, no stdout)
/// must not block the tool; the shell body runs normally.
#[tokio::test]
#[serial]
async fn broken_pre_hook_fails_open() {
    let port = free_port();
    let sentinel = sentinel_path("open", port);
    let _ = std::fs::remove_file(&sentinel);

    let hooks = write_hooks_file(
        port,
        json!({
            "PreToolUse": [
                { "matcher": "*", "hooks": [ { "type": "command", "command": "exit 1" } ] }
            ]
        }),
    );
    let mock = shell_cmd_mock(&format!("echo open-ran > '{}'", sentinel.display()));
    start_hooked_gateway(port, hooks, mock.clone()).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the shell tool to write a marker file.")
        .await;
    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("Expected chat.final event");

    let content =
        std::fs::read_to_string(&sentinel).expect("shell body must run despite broken hook");
    assert_eq!(content.trim(), "open-ran");
}

/// Case 6 — PreToolUse ask 升级审批: a hook returning `{"permission":"ask"}`
/// suspends the turn on the approval queue; the tool body runs only after
/// the operator approves through the WS methods.
#[tokio::test]
#[serial]
async fn pre_tool_use_ask_suspends_until_approved() {
    let port = free_port();
    let sentinel = sentinel_path("ask", port);
    let _ = std::fs::remove_file(&sentinel);

    let hooks = write_hooks_file(
        port,
        json!({
            "PreToolUse": [
                { "matcher": "*", "hooks": [ {
                    "type": "command",
                    "command": "printf '{\"permission\":\"ask\",\"reason\":\"hook-asks\"}'"
                } ] }
            ]
        }),
    );
    let mock = shell_cmd_mock(&format!("echo asked-ran > '{}'", sentinel.display()));
    start_hooked_gateway_opts(port, hooks, mock.clone(), false).await;

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client
        .send_chat(&sid, "Use the shell tool to write a sentinel file.")
        .await;

    // The hook's ask becomes an approval request citing the hook reason…
    let payload = client
        .wait_for_event("approval.required", 30)
        .await
        .expect("the hook's ask must raise an approval request");
    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        message.contains("hook-asks"),
        "the approval must carry the hook's reason, got: {message}"
    );
    assert!(!sentinel.exists(), "tool body must not run while pending");

    // …approving resumes the turn and the tool body runs.
    client
        .request("approvals.approve", json!({ "id": payload["approval_id"] }))
        .await;
    client
        .wait_for_event("chat.final", 30)
        .await
        .expect("turn must complete after approval");
    assert_eq!(
        std::fs::read_to_string(&sentinel)
            .expect("approved body must have run")
            .trim(),
        "asked-ran"
    );
}
