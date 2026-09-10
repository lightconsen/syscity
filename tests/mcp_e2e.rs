//! MCP End-to-End Tests — connect to a mock stdio MCP server.

use std::path::PathBuf;

use syscity::mcp::{McpManager, McpServerConfig, McpTransport};
use tokio::time::{timeout, Duration};

/// Helper: skip test gracefully if `python3` is not available.
async fn ensure_python3() {
    if tokio::process::Command::new("python3")
        .arg("--version")
        .output()
        .await
        .is_err()
    {
        eprintln!("Skipping MCP E2E test: python3 not available");
        std::process::exit(0);
    }
}

#[tokio::test]
async fn test_mcp_manager_connects_to_mock_server_and_lists_tools() {
    ensure_python3().await;

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mock_server = manifest.join("tests/fixtures/mock_mcp_server.py");

    let config = McpServerConfig {
        transport: McpTransport::Stdio,
        command: Some("python3".to_string()),
        args: vec![mock_server.to_string_lossy().to_string()],
        ..Default::default()
    };

    let manager = McpManager::default();
    let tools = timeout(Duration::from_secs(10), manager.connect("mock", config))
        .await
        .expect("timed out waiting for MCP connection")
        .expect("MCP connect failed");

    assert_eq!(tools.len(), 1, "Expected 1 tool from mock server");
    assert_eq!(tools[0].name, "echo");

    let servers = manager.list_servers().await;
    assert!(servers.contains(&"mock".to_string()));

    manager.disconnect("mock").await.expect("disconnect failed");
    let servers = manager.list_servers().await;
    assert!(!servers.contains(&"mock".to_string()));
}

#[tokio::test]
async fn test_mcp_manager_reconnects_after_disconnect() {
    ensure_python3().await;

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mock_server = manifest.join("tests/fixtures/mock_mcp_server.py");

    let config = McpServerConfig {
        transport: McpTransport::Stdio,
        command: Some("python3".to_string()),
        args: vec![mock_server.to_string_lossy().to_string()],
        ..Default::default()
    };

    let manager = McpManager::default();

    // First connect
    let tools1 = timeout(Duration::from_secs(10), manager.connect("mock", config.clone()))
        .await
        .expect("timed out")
        .expect("first connect failed");
    assert_eq!(tools1.len(), 1);

    // Disconnect
    manager.disconnect("mock").await.unwrap();

    // Reconnect with backoff helper (reconnect_with_backoff is not public,
    // but we can call connect again directly).
    let tools2 = timeout(Duration::from_secs(10), manager.connect("mock", config))
        .await
        .expect("timed out")
        .expect("reconnect failed");
    assert_eq!(tools2.len(), 1);
}
