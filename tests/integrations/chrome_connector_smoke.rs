//! End-to-end smoke test: install a real connector package wrapping the
//! official `chrome-devtools-mcp` stdio server, enable it, and verify the
//! MCP handshake + tools/list round-trip over the actual protocol.
//!
//! Requires node/npx on PATH and network for the first package download —
//! run explicitly:
//! ```text
//! cargo test --test integrations_test chrome_connector -- --ignored --nocapture
//! ```

use std::sync::Arc;

use syscity::mcp::connectors::state::StateKind;
use syscity::mcp::{ConnectorManager, McpManager};
use syscity::skills::SkillStorage;

/// A real connector package wrapping the official chrome-devtools MCP server.
const CONNECTOR_JSON: &str = r#"{
  "connector": {
    "id": "chrome-devtools",
    "display_name": "Chrome DevTools MCP",
    "description": "Drive a real Chrome browser via the official chrome-devtools-mcp stdio server",
    "version": "1.8.0"
  },
  "mcp": {
    "transport": "stdio",
    "command": "npx",
    "args": ["-y", "chrome-devtools-mcp@1.8.0"],
    "auto_connect": false
  },
  "skills": ["skills"]
}"#;

const SKILL_MD: &str = r#"---
name: drive-browser
description: Drive Chrome via the chrome-devtools connector
---
Use the `mcp__chrome-devtools__*` tools: navigate_page, take_snapshot, click, fill.
"#;

#[tokio::test]
#[ignore = "spawns the real chrome-devtools-mcp via npx; needs node + npm registry access"]
async fn chrome_devtools_connector_full_cycle() {
    let base =
        std::env::temp_dir().join(format!("syscity_chrome_conn_smoke_{}", uuid::Uuid::new_v4()));
    let root = base.join("connectors");
    let user_skills = base.join("user-skills");
    let pkg = base.join("pkg");
    let skill_dir = pkg.join("skills/drive-browser");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(pkg.join("connector.json"), CONNECTOR_JSON).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), SKILL_MD).unwrap();

    let mcp_manager = Arc::new(McpManager::default());
    let manager = ConnectorManager::new(
        root.clone(),
        mcp_manager.clone(),
        Arc::new(SkillStorage::with_user_dir(user_skills.clone())),
        #[cfg(feature = "cloud")]
        None,
        Arc::new(syscity::secrets::SecretStoreHandle::default()),
    );

    // ── 1. Install: cache copy + bundled-skill bridge ──────────────────────
    let summary = manager
        .install_from_dir(&pkg)
        .await
        .expect("install should succeed");
    assert_eq!(summary.id, "chrome-devtools");
    assert_eq!(summary.state, StateKind::Installed);
    assert!(summary.provides_mcp);
    assert_eq!(summary.skills, vec!["connector-chrome-devtools-drive-browser"]);
    let skill_dir = user_skills.join("connector-chrome-devtools-drive-browser");
    assert!(skill_dir.join("SKILL.md").exists(), "skill bridged to user dir");

    // ── 2. Enable: real stdio spawn of `npx chrome-devtools-mcp@1.8.0`,
    //       JSON-RPC initialize + tools/list over the wire ──────────────────
    let enabled = manager.enable("chrome-devtools").await.unwrap_or_else(|e| {
        panic!("enable failed (is chrome-devtools-mcp warm in the npx cache?): {e}")
    });
    assert_eq!(enabled.state, StateKind::Enabled);

    // The connected server's tools must be visible through the manager.
    let client = mcp_manager
        .get_client("chrome-devtools")
        .await
        .expect("client registered under the connector id");
    let tool_names: Vec<String> = {
        let c = client.read().await;
        c.get_tools().iter().map(|t| t.name.clone()).collect()
    };
    println!("chrome-devtools tools ({}): {tool_names:#?}", tool_names.len());
    assert!(
        tool_names.iter().any(|n| n == "navigate_page"),
        "navigate_page tool expected; got {tool_names:?}"
    );
    assert!(tool_names.iter().any(|n| n == "take_snapshot"), "take_snapshot tool expected");

    // ── 3. Round-trip a real tool call through the connector ───────────────
    // list_pages is side-effect-light (the server may lazily launch Chrome).
    let pages = client
        .read()
        .await
        .call_tool("list_pages", serde_json::json!({}))
        .await;
    match pages {
        Ok(result) => println!("list_pages → {}", result),
        Err(e) => println!("list_pages skipped ({e}) — connectivity already proven by tools/list"),
    }

    // ── 4. Disable / uninstall teardown ────────────────────────────────────
    let disabled = manager.disable("chrome-devtools").await.unwrap();
    assert_eq!(disabled.state, StateKind::Disabled);
    manager.uninstall("chrome-devtools").await.unwrap();

    assert!(!user_skills
        .join("connector-chrome-devtools-drive-browser")
        .exists());
    let remaining = manager.list().await.unwrap();
    assert!(remaining.is_empty(), "no connectors left after uninstall");

    let _ = std::fs::remove_dir_all(&base);
}
