//! E2E tests for platform-specific tools (macOS / Linux / Windows)
//!
//! Each test verifies that a platform tool can be invoked via chat and returns
//! valid results.  Platform-gated with `#[cfg_attr(not(target_os = "..."),
//! ignore)]`.
//!
//! Prerequisites:
//! - A configured LLM provider: `SYSCITY_TEST_PROVIDER_KEY` +
//!   `SYSCITY_TEST_PROVIDER` env vars, or `start-local-qwen.sh` /
//!   `start-local-kimi.sh` in project root
//! - Platform-specific permissions (Screen Recording, Accessibility, etc.)
//!
//! Run macOS tests:
//!   cargo test --test e2e_test tool_macos_ -- --include-ignored --nocapture
//!
//! Run Linux tests:
//!   cargo test --test e2e_test tool_linux_ -- --include-ignored --nocapture
//!
//! Run Windows tests:
//!   cargo test --test e2e_test tool_windows_ -- --include-ignored --nocapture

use super::*;

// ── Multi-tool Chat Test Helper ─────────────────────────────────────────────

/// Run a chat test that accepts multiple expected tool names.
///
/// Collects *all* tool invocations across the conversation.  With a real LLM
/// provider all expected tools are verified; with the mock fallback only the
/// first expected tool is checked (the mock only produces one turn).
async fn run_multi_tool_chat_test(
    port: u16,
    prompt: &str,
    expected_tools: &[&str],
) -> Vec<serde_json::Value> {
    let has_real = pick_test_provider().is_some();

    if has_real {
        start_test_gateway(port, true).await;
    } else {
        // Mock only produces a single tool call.
        start_test_gateway_with_mock(port, tool_mock_provider(expected_tools[0])).await;
    }

    let mut client = FrontendSimulator::connect(port).await;
    let sid = client.create_session().await;
    client.subscribe(vec![sid.clone()]).await;
    client.send_chat(&sid, prompt).await;

    let result = timeout(Duration::from_secs(120), async {
        let mut called: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut tool_results = Vec::new();

        while let Some(msg) = client.read.next().await {
            let msg = msg.unwrap();
            if let Message::Text(text) = msg {
                if let Ok(ev) = serde_json::from_str::<serde_json::Value>(&text) {
                    if ev.get("type").and_then(|v| v.as_str()) != Some("event") {
                        continue;
                    }
                    let name = ev.get("event").and_then(|v| v.as_str());
                    let payload = ev.get("payload").cloned();
                    match name {
                        Some("tool.calling") => {
                            if let Some(ref p) = payload {
                                if let Some(t) = p.get("tool_name").and_then(|v| v.as_str()) {
                                    called.insert(t.to_string());
                                }
                            }
                        }
                        Some("tool.result") => {
                            if let Some(p) = payload {
                                tool_results.push(p);
                            }
                        }
                        Some("chat.final") => break,
                        _ => {}
                    }
                }
            }
        }
        (called, tool_results)
    })
    .await;

    let (called, tool_results) = result.expect("Timed out waiting for chat.final");

    if has_real {
        for expected in expected_tools {
            assert!(
                called.contains(*expected),
                "Expected tool '{expected}' to be called. Called: {:?}",
                called,
            );
        }
    } else {
        // With mock we only verify the first tool was invoked.
        assert!(
            called.contains(expected_tools[0]),
            "Expected tool '{}' to be called (mock mode). Called: {:?}",
            expected_tools[0],
            called,
        );
    }

    tool_results
}

// ── Result Assertion Helpers ────────────────────────────────────────────────
//
// The `tool.result` event payload has this shape:
// {
//   "session_id": "...",
//   "agent_id": "...",
//   "tool_name": "macos_screenshot",
//   "result": "Screenshot captured (12345 bytes, base64: data:...)",  // String
// (truncated 200 chars)   "data": { "image_base64": "...", "format": "jpeg",
// ... }         // ToolExecutionResult.data }

fn assert_screenshot_data(result: &serde_json::Value) {
    let data = result.get("data").and_then(|v| v.as_object());
    assert!(data.is_some(), "top-level 'data' should contain screenshot metadata");
    if let Some(d) = data {
        assert!(d.contains_key("image_base64"), "'data.image_base64' missing");
        if let Some(b64) = d.get("image_base64").and_then(|v| v.as_str()) {
            assert!(b64.len() > 100, "base64 too short: {}", b64.len());
        }
        if let Some(sz) = d.get("size").and_then(|v| v.as_u64()) {
            assert!(sz > 1000, "screenshot file size too small: {sz}");
        }
        assert!(d.get("format").is_some(), "'data.format' missing");
    }
}

fn assert_accessibility_data(result: &serde_json::Value) {
    let data = result.get("data").and_then(|v| v.as_object());
    assert!(data.is_some(), "top-level 'data' should contain accessibility result");
    if let Some(d) = data {
        // Check that either it succeeded with elements, or failed with an error
        match d.get("success").and_then(|v| v.as_bool()) {
            Some(true) => {
                assert!(d.contains_key("elements"), "accessibility success=true but no 'elements'");
            }
            Some(false) => {
                // Failed — should have an error message
                assert!(d.contains_key("error"), "accessibility failed but no 'error' field");
            }
            None => {
                // Unknown format — just check elements exists or raw_output exists
                assert!(
                    d.contains_key("elements") || d.contains_key("raw_output"),
                    "accessibility data should have 'elements' or 'raw_output'"
                );
            }
        }
    }
}

fn assert_applescript_data(result: &serde_json::Value) {
    let data = result.get("data").and_then(|v| v.as_object());
    if let Some(d) = data {
        if let Some(success) = d.get("success").and_then(|v| v.as_bool()) {
            assert!(success, "AppleScript should succeed");
        }
    }
}

fn assert_process_output(result: &serde_json::Value) {
    let output = result.get("result").and_then(|v| v.as_str()).unwrap_or("");
    assert!(!output.is_empty(), "process tool should produce non-empty 'result' string");
}

// ════════════════════════════════════════════════════════════════════════════
// 1. 截图感知
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 帮我截个屏，告诉我当前屏幕上有什么。
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_macos_screenshot_basic() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the macos_screenshot tool to take a screenshot and describe what you see on my \
         screen. Call ONLY macos_screenshot.",
        "macos_screenshot",
    )
    .await;
    for r in &results {
        assert_screenshot_data(r);
    }
}

/// Prompt: 截取当前最前面的窗口，描述一下它的内容和布局。
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_macos_screenshot_front_window() {
    let results = run_tool_chat_test(
        free_port(),
        "Capture the frontmost window using macos_screenshot. Set the window parameter to capture \
         a specific window. Describe its content and layout. Call ONLY macos_screenshot.",
        "macos_screenshot",
    )
    .await;
    for r in &results {
        assert_screenshot_data(r);
    }
}

/// Prompt: 打开我的浏览器，截个图看看我在看什么网页。
#[tokio::test]
#[serial]
#[ignore = "Opens browser on the desktop"]
async fn tool_macos_screenshot_after_open_browser() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "Use applescript to open Google Chrome or Safari. Then use macos_screenshot to take a \
         screenshot and tell me what web page is currently open.",
        &["applescript", "macos_screenshot"],
    )
    .await;
    for r in &results {
        if r.get("tool_name").and_then(|v| v.as_str()) == Some("macos_screenshot") {
            assert_screenshot_data(r);
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 2. 读取 UI 树（无障碍）
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 读取当前前端应用的 UI 结构，列出所有按钮和文本框。
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_macos_accessibility_ui_tree() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the macos_accessibility tool to read the frontmost application's UI structure. List \
         all visible buttons and text fields. Call ONLY macos_accessibility.",
        "macos_accessibility",
    )
    .await;
    for r in &results {
        assert_accessibility_data(r);
    }
}

/// Prompt: 用 macos_accessibility 读取 Finder
/// 的窗口结构，告诉我里面有哪些文件夹可见。
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_macos_accessibility_finder() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the macos_accessibility tool to read the Finder application's window structure. Tell \
         me what folders and files are visible. Call ONLY macos_accessibility.",
        "macos_accessibility",
    )
    .await;
    for r in &results {
        assert_accessibility_data(r);
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 3. 桌面控制 —— 打开应用
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 帮我打开系统设置，然后用 macos_desktop_control
/// 告诉我设置窗口里有哪些选项。
#[tokio::test]
#[serial]
#[ignore = "Requires display server + accessibility permissions"]
async fn tool_macos_desktop_control_open_settings() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "First use applescript to open System Settings (System Preferences). Then use \
         macos_desktop_control with action=inspect to read the settings window. Tell me what \
         options are available.",
        &["applescript", "macos_desktop_control"],
    )
    .await;
    assert!(!results.is_empty(), "expected at least one tool result");
}

/// Prompt: 打开 Chrome 浏览器，搜索 "syscity"，然后把结果截图给我看。
#[tokio::test]
#[serial]
#[ignore = "Requires Chrome + display server; performs real web search"]
async fn tool_macos_desktop_control_chrome_search() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "Use applescript to open Google Chrome and navigate to a search for 'syscity'. Then use \
         macos_screenshot to capture the browser window showing the search results.",
        &["applescript", "macos_screenshot"],
    )
    .await;
    for r in &results {
        if r.get("tool_name").and_then(|v| v.as_str()) == Some("macos_screenshot") {
            assert_screenshot_data(r);
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 4. 桌面控制 —— 点击和输入
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 打开备忘录应用，新建一条笔记，输入 "Hello from
/// Syscity"，然后把内容截图给我看。
#[tokio::test]
#[serial]
#[ignore = "Modifies actual desktop state (creates Notes content)"]
async fn tool_macos_desktop_control_notes() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "Use applescript to open the Notes app and create a new note with the content 'Hello from \
         Syscity'. Then use macos_screenshot to capture the Notes window to confirm.",
        &["applescript", "macos_screenshot"],
    )
    .await;
    for r in &results {
        if r.get("tool_name").and_then(|v| v.as_str()) == Some("macos_screenshot") {
            assert_screenshot_data(r);
        }
    }
}

/// Prompt: 帮我打开计算器，计算 128 × 256，把结果截图发给我。
#[tokio::test]
#[serial]
#[ignore = "Modifies actual desktop state (opens Calculator)"]
async fn tool_macos_desktop_control_calculator() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "Use applescript to open the Calculator app. Then use macos_desktop_control to press the \
         buttons for 128 × 256 =. Finally use macos_screenshot to capture the result.",
        &["applescript", "macos_desktop_control", "macos_screenshot"],
    )
    .await;
    assert!(!results.is_empty(), "expected at least one tool result");
}

// ════════════════════════════════════════════════════════════════════════════
// 5. 进程管理
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 查看当前系统有哪些进程在占用最多 CPU。
#[tokio::test]
#[serial]
async fn tool_process_cpu() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the process tool to list running processes sorted by CPU usage. Tell me which \
         processes are consuming the most CPU. Call ONLY process.",
        "process",
    )
    .await;
    for r in &results {
        assert_process_output(r);
    }
}

/// Prompt: 帮我检查一下 Chrome 浏览器当前打开了多少个进程。
#[tokio::test]
#[serial]
async fn tool_process_chrome() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the process tool to find and count how many Chrome processes are currently running. \
         Call ONLY process.",
        "process",
    )
    .await;
    for r in &results {
        assert_process_output(r);
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 6. 文件操作 + 感知组合
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 在我的桌面上创建一个 "test_output" 文件夹，创建完成后截个图验证。
#[tokio::test]
#[serial]
#[ignore = "Creates a folder on the actual desktop"]
async fn tool_combo_folder_and_screenshot() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "First use the shell tool to create a folder called 'test_output' on my desktop (mkdir -p \
         ~/Desktop/test_output). Then use macos_screenshot to take a screenshot to verify the \
         folder was created.",
        &["shell", "macos_screenshot"],
    )
    .await;
    for r in &results {
        if r.get("tool_name").and_then(|v| v.as_str()) == Some("macos_screenshot") {
            assert_screenshot_data(r);
        }
    }
    // Cleanup
    let _ = std::process::Command::new("rm")
        .args([
            "-rf",
            &format!("{}/Desktop/test_output", std::env::var("HOME").unwrap_or_default()),
        ])
        .output();
}

/// Prompt: 列出 Downloads 文件夹里最大的 5 个文件，然后截图显示文件夹内容。
#[tokio::test]
#[serial]
#[ignore = "Requires display server for screenshot"]
async fn tool_combo_list_files_and_screenshot() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "First use the shell tool to list the 5 largest files in my Downloads folder (ls -lhS \
         ~/Downloads | head -6). Then use macos_screenshot to show the Downloads folder contents.",
        &["shell", "macos_screenshot"],
    )
    .await;
    assert!(!results.is_empty(), "expected at least one tool result");
}

// ════════════════════════════════════════════════════════════════════════════
// 7. AppleScript 深度控制
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 用 AppleScript 执行 shell 命令，验证 applescript 工具可用。
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_applescript_finder_path() {
    let results = run_tool_chat_test(
        free_port(),
        "Use ONLY the applescript tool. Execute: do shell script \"echo syscity_test_ok\". Report \
         the result. Do not use shell or any other tool.",
        "applescript",
    )
    .await;
    for r in &results {
        assert_applescript_data(r);
    }
}

/// Prompt: 用 apple script 弹出 "Hello from Syscity" 的对话框。
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_applescript_dialog() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the applescript tool to display a dialog box saying 'Hello from Syscity'. Script: \
         display dialog \"Hello from Syscity\" buttons {\"OK\"} default button \"OK\". Call ONLY \
         applescript.",
        "applescript",
    )
    .await;
    for r in &results {
        assert_applescript_data(r);
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 8. 全流程 —— 组合多个工具
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: 先截图看看我桌面当前状态，然后读取 Safari 的 UI 树结构，最后告诉我
/// Safari 里打开了什么。
#[tokio::test]
#[serial]
#[ignore = "Requires Safari + display server"]
async fn tool_combo_safari_screenshot_and_ui_tree() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "First use macos_screenshot to capture my current desktop. Then use macos_accessibility \
         to read Safari's UI tree and tell me what web pages are open in Safari.",
        &["macos_screenshot", "macos_accessibility"],
    )
    .await;
    for r in &results {
        match r.get("tool_name").and_then(|v| v.as_str()) {
            Some("macos_screenshot") => assert_screenshot_data(r),
            Some("macos_accessibility") => assert_accessibility_data(r),
            _ => {}
        }
    }
}

/// Prompt: 执行以下步骤：（1）截图当前桌面；（2）查看当前运行的进程；
/// （3）读取最前端应用的 UI 树。 汇总成一个报告给我。
#[tokio::test]
#[serial]
#[ignore = "Requires display server"]
async fn tool_combo_full_desktop_report() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "Execute these three steps and give me a summary report: 1. Use macos_screenshot to \
         capture the current desktop 2. Use the process tool to list running processes 3. Use \
         macos_accessibility to read the frontmost application's UI tree",
        &["macos_screenshot", "process", "macos_accessibility"],
    )
    .await;

    // Verify every expected tool was called at least once
    let tool_names: std::collections::HashSet<&str> = results
        .iter()
        .filter_map(|r| r.get("tool_name").and_then(|v| v.as_str()))
        .collect();
    for expected in &["macos_screenshot", "process", "macos_accessibility"] {
        assert!(
            tool_names.contains(expected),
            "Expected '{expected}' to be called in full-flow test. Called: {:?}",
            tool_names,
        );
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Retained from the first batch
// ════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_macos_screenshot_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the macos_screenshot tool to take a screenshot and tell me what you see. Call ONLY \
         macos_screenshot.",
        "macos_screenshot",
    )
    .await;
    for r in &results {
        assert_screenshot_data(r);
    }
}

#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_macos_accessibility_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the macos_accessibility tool to read the frontmost application's UI tree. List all \
         visible buttons, text fields, and windows. Call ONLY macos_accessibility.",
        "macos_accessibility",
    )
    .await;
    for r in &results {
        assert_accessibility_data(r);
    }
}

#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "macos"), ignore = "macOS-only test")]
async fn tool_applescript_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the applescript tool to execute: tell application \"Finder\" to get name of every \
         window. Report the output. Call ONLY applescript.",
        "applescript",
    )
    .await;
    for r in &results {
        assert_applescript_data(r);
    }
}

#[tokio::test]
#[serial]
#[ignore = "Requires display server + accessibility permissions; may interact with desktop"]
async fn tool_macos_desktop_control_inspect() {
    let results = run_tool_chat_test(
        free_port(),
        "Use the macos_desktop_control tool with action=inspect to inspect the current desktop \
         state. Tell me what applications are open and what UI elements are visible. Call ONLY \
         macos_desktop_control.",
        "macos_desktop_control",
    )
    .await;
    for r in &results {
        let data = r.get("data").and_then(|v| v.as_object());
        if let Some(d) = data {
            assert!(
                d.contains_key("success") || d.contains_key("mode"),
                "result.data should contain 'success' or 'mode'"
            );
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 9. Linux — system-level tools
// ════════════════════════════════════════════════════════════════════════════

/// Prompt: inspect system information.
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore = "Linux-only test")]
async fn tool_linux_system_inspect_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use ONLY the system_inspect tool. Get the hostname and uptime of this Linux system. Do \
         not use any other tool.",
        "system_inspect",
    )
    .await;
    for r in &results {
        let result_str = r.get("result").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!result_str.is_empty(), "system_inspect should produce output");
    }
}

/// Prompt: send a desktop notification.
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore = "Linux-only test")]
async fn tool_linux_notification_invoked_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use ONLY the linux_notification tool. Send a notification with summary='Syscity Test' \
         and body='E2E notification test'. Do not use any other tool.",
        "linux_notification",
    )
    .await;
    for r in &results {
        let result_str = r.get("result").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!result_str.is_empty(), "linux_notification should produce output");
    }
}

/// Prompt: combine system inspection with process listing.
#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "linux"), ignore = "Linux-only test")]
async fn tool_linux_multi_system_and_process() {
    let results = run_multi_tool_chat_test(
        free_port(),
        "First use the system_inspect tool to get the hostname and uptime. Then use the process \
         tool to list the top 5 processes by CPU.",
        &["system_inspect", "process"],
    )
    .await;
    assert!(!results.is_empty(), "expected at least one tool result");
}

// ════════════════════════════════════════════════════════════════════════════
// 10. Windows — desktop automation tools
// ════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
#[cfg_attr(not(target_os = "windows"), ignore = "Windows-only test")]
async fn tool_windows_clipboard_invoked_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use ONLY the windows_clipboard tool. Get the current clipboard content. Do not use any \
         other tool.",
        "windows_clipboard",
    )
    .await;
    for r in &results {
        let result_str = r.get("result").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!result_str.is_empty(), "windows_clipboard should produce output");
    }
}

#[tokio::test]
#[serial]
#[ignore = "Requires display server + desktop permissions"]
async fn tool_windows_screenshot_invoked_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use ONLY the windows_screenshot tool. Take a screenshot of the primary display. Do not \
         use any other tool.",
        "windows_screenshot",
    )
    .await;
    for r in &results {
        let result_str = r.get("result").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!result_str.is_empty(), "windows_screenshot should produce output");
    }
}

#[tokio::test]
#[serial]
#[ignore = "Requires display server + desktop permissions (Windows-only test)"]
async fn tool_windows_desktop_control_invoked_via_chat() {
    let results = run_tool_chat_test(
        free_port(),
        "Use ONLY the windows_desktop_control tool. List all open windows. Do not use any other \
         tool.",
        "windows_desktop_control",
    )
    .await;
    for r in &results {
        let result_str = r.get("result").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!result_str.is_empty(), "windows_desktop_control should produce output");
    }
}
