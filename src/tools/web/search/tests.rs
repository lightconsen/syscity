//! Search-provider selection tests for the web search tool.

// Tests assert against fallible bind/send/read results; unwrapping keeps
// the failure-path assertions readable (same allowance as
// `sandbox_interceptor`).
#![allow(clippy::unwrap_used)]

use super::*;
use crate::tools::{Tool, ToolContext, ToolExecutionResult};

#[test]
fn test_web_search_tool_creation() {
    let tool = WebSearchTool::new();
    assert_eq!(tool.name(), "web_search");
    assert!(!tool.description().is_empty());
}

#[test]
fn test_parse_duckduckgo_results() {
    let html = r#"
        <div class="result">
            <a rel="nofollow" href="http://example.com">Test Title</a>
            <a class="result__snippet">Test snippet here</a>
        </div>
        <div class="result">
            <a rel="nofollow" href="http://example2.com">Second Title</a>
            <a class="result__snippet">Second snippet</a>
        </div>
    "#;

    let results = WebSearchTool::parse_duckduckgo_results(html, 10);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Test Title");
    assert_eq!(results[0].url, "http://example.com");
    assert_eq!(results[0].snippet, "Test snippet here");
}

#[test]
fn test_clean_html() {
    assert_eq!(WebSearchTool::clean_html("Hello &amp; World"), "Hello & World");
    assert_eq!(WebSearchTool::clean_html("&lt;tag&gt;"), "<tag>");
    assert_eq!(WebSearchTool::clean_html("<b>Bold</b>"), "Bold");
}

// ── Search: selection-failure classification ────────────────────────────

/// Bind an ephemeral port and immediately drop it so connections are
/// deterministically refused.
async fn dead_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn run_search(tool: &WebSearchTool) -> ToolExecutionResult {
    tool.execute(serde_json::json!({ "query": "syscity" }), &ToolContext::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn search_empty_provider_list_reports_not_configured() {
    let tool = WebSearchTool::new().with_providers(Vec::new());
    let result = run_search(&tool).await;
    assert!(!result.success);
    assert_eq!(result.data.unwrap()["code"], "NOT_CONFIGURED");
}

#[tokio::test]
async fn search_provider_without_key_reports_configured_missing() {
    let tool = WebSearchTool::new().with_provider(SearchProvider::Brave { api_key: String::new() });
    let result = run_search(&tool).await;
    assert!(!result.success);
    let data = result.data.unwrap();
    assert_eq!(data["code"], "CONFIGURED_MISSING");
    assert_eq!(data["attempts"][0]["provider"], "brave");
    assert_eq!(data["attempts"][0]["outcome"], "configured_missing");
}

#[tokio::test]
async fn search_dead_endpoint_reports_unavailable() {
    let port = dead_port().await;
    let tool = WebSearchTool::new().with_provider(SearchProvider::Custom {
        url: format!("http://127.0.0.1:{}/{{query}}", port),
        api_key: None,
        headers: None,
        result_parser: None,
    });
    let result = run_search(&tool).await;
    assert!(!result.success);
    let data = result.data.unwrap();
    assert_eq!(data["code"], "UNAVAILABLE");
    assert_eq!(data["attempts"][0]["outcome"], "unavailable");
}

#[tokio::test]
async fn search_mixed_failure_classes_report_ambiguous() {
    let port = dead_port().await;
    let tool = WebSearchTool::new().with_providers(vec![
        SearchProvider::Brave { api_key: String::new() },
        SearchProvider::Custom {
            url: format!("http://127.0.0.1:{}/{{query}}", port),
            api_key: None,
            headers: None,
            result_parser: None,
        },
    ]);
    let result = run_search(&tool).await;
    assert!(!result.success);
    let data = result.data.unwrap();
    assert_eq!(data["code"], "AMBIGUOUS");
    assert_eq!(data["attempts"].as_array().unwrap().len(), 2);
}
