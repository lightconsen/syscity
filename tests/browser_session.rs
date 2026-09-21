//! Browser session integration tests.
//!
//! These drive a real headless Chrome through the browser pool, so they only
//! run when explicitly asked for:
//!
//! ```text
//! SYSCITY_BROWSER_E2E=1 cargo test --test browser_session
//! ```
//!
//! Ordinary `cargo test` runs compile them but skip the bodies — no Chrome
//! needed. The pages they visit are `data:` URLs: nothing leaves the machine.

#![cfg(feature = "browser")]

use serde_json::json;
use std::sync::Arc;

use syscity::browser::pool::BrowserPool;
use syscity::browser::profile::{BrowserPoolConfig, BrowserProfile};
use syscity::tools::browser::BrowserTool;
use syscity::tools::{Tool, ToolContext, ToolIdentity, ToolModel, ToolSandbox};

fn e2e_enabled() -> bool {
    std::env::var("SYSCITY_BROWSER_E2E").is_ok_and(|v| v == "1")
}

fn tool_context() -> ToolContext {
    ToolContext {
        identity: ToolIdentity {
            user_id: "e2e".to_string(),
            conversation_id: "e2e".to_string(),
            sender_id: None,
        },
        sandbox: ToolSandbox::default(),
        model: ToolModel::default(),
        delegation: None,
        ask_queue: None,
        approval_queue: None,
    }
}

/// A tool with a live pool and a long idle timeout, so an instance is not
/// evicted between the two calls a test makes. Each test gets its own
/// `user_data_dir`: Chrome aborts a second launch that shares a profile
/// directory (`ProcessSingleton`), which otherwise collides when tests run
/// in parallel.
fn pooled_tool(name: &str) -> (BrowserTool, Arc<BrowserPool>) {
    let dir =
        std::env::temp_dir().join(format!("syscity-e2e-browser-{}-{}", name, std::process::id()));
    let mut profile = BrowserProfile::new(name);
    profile.user_data_dir = Some(dir);
    let config = BrowserPoolConfig {
        idle_timeout_secs: 600,
        default_profile: name.to_string(),
        ..BrowserPoolConfig::default()
    };
    let pool = Arc::new(BrowserPool::with_profiles(config, vec![profile]));
    let tool = BrowserTool::new()
        .with_profile(name)
        .with_pool(Arc::clone(&pool));
    (tool, pool)
}

const CONTENT_PAGE: &str = "data:text/html,<title>persist</title><p>survives</p>";

/// The regression behind the empty-page failures: a Navigate in one tool call
/// and a read in the next must land on the SAME page. Every call used to open
/// a fresh `about:blank`, so the second call saw an empty document, the Type
/// found no element, and the reads returned empty strings.
#[tokio::test(flavor = "multi_thread")]
async fn page_state_survives_between_tool_calls() {
    if !e2e_enabled() {
        eprintln!("skipped: set SYSCITY_BROWSER_E2E=1 to run browser e2e tests");
        return;
    }
    let (tool, _pool) = pooled_tool("persist");
    let context = tool_context();

    let navigate = tool
        .execute(json!({ "actions": [{ "Navigate": { "url": CONTENT_PAGE } }] }), &context)
        .await
        .expect("navigate executes");
    assert!(navigate.success, "navigate failed: {:?}", navigate.error);

    let read = tool
        .execute(json!({ "actions": [{ "GetText": {} }] }), &context)
        .await
        .expect("get_text executes");
    assert!(read.success, "get_text failed: {:?}", read.error);
    assert!(
        read.output.contains("survives"),
        "the page did not persist across tool calls: {}",
        read.output
    );
}

/// A GetHtml written as `{"GetHtml": {}}` — the exact shape the tool's own
/// schema advertises — must parse and read the live page. Before the parse
/// fallback it failed with "invalid type: map, expected unit" while the same
/// shape parsed for variants whose fields are all optional.
#[tokio::test(flavor = "multi_thread")]
async fn get_html_as_the_documented_empty_object_shape_works() {
    if !e2e_enabled() {
        eprintln!("skipped: set SYSCITY_BROWSER_E2E=1 to run browser e2e tests");
        return;
    }
    let (tool, _pool) = pooled_tool("shapes");
    let context = tool_context();

    tool.execute(json!({ "actions": [{ "Navigate": { "url": CONTENT_PAGE } }] }), &context)
        .await
        .expect("navigate executes");

    let html = tool
        .execute(json!({ "actions": [{ "GetHtml": {} }] }), &context)
        .await
        .expect("get_html executes");
    assert!(html.success, "get_html failed: {:?}", html.error);
    assert!(
        html.output.contains("survives"),
        "the empty-object GetHtml shape did not reach the live page: {}",
        html.output
    );
}

/// A tracked page that died since the last call (closed externally, crashed)
/// must be detected and replaced, not handed to the next call.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_tracked_page_is_replaced_not_reused() {
    if !e2e_enabled() {
        eprintln!("skipped: set SYSCITY_BROWSER_E2E=1 to run browser e2e tests");
        return;
    }
    let (tool, pool) = pooled_tool("dead-page");
    let context = tool_context();

    tool.execute(json!({ "actions": [{ "Navigate": { "url": CONTENT_PAGE } }] }), &context)
        .await
        .expect("navigate executes");

    // Close the tracked page out from under the tool.
    let instance = pool.get_or_create("dead-page").await.expect("instance");
    let dead = instance.most_recent_page().await.expect("tracked page");
    assert!(instance.close_page(&dead.target_id).await.expect("closes"), "page closed");

    let next = tool
        .execute(json!({ "actions": [{ "Navigate": { "url": CONTENT_PAGE } }] }), &context)
        .await
        .expect("navigate after closing the tracked page");
    assert!(
        next.success,
        "a dead tracked page must not poison the next call: {:?}",
        next.error
    );
}
